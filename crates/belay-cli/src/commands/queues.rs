//! `belay queue ...`: queue status, pausing, purging and configuration.

use belay_common::format_duration;
use belay_json::{json, ToJson, Value};
use belay_proto::{JobState, QueueConfig, QueueInfo, QueueName};

use super::Ctx;
use crate::args::{Arg, Command, Flag, KeyValue, Matches};
use crate::error::Result;
use crate::input::{self, Origin};
use crate::output::human::approx;
use crate::output::{state_style, Fields, Grid};

pub(crate) fn spec() -> Command {
    let name = || Arg::required("name", "Queue name");
    Command::new("queue", "inspect and manage queues")
        .subcommand(Command::new("list", "list queues with their job counts").alias("ls"))
        .subcommand(
            Command::new("info", "show a queue's counts, throughput and configuration")
                .alias("get")
                .arg(name()),
        )
        .subcommand(
            Command::new("pause", "stop handing out jobs from a queue (enqueueing continues)")
                .arg(name()),
        )
        .subcommand(Command::new("resume", "hand out jobs from a paused queue again").arg(name()))
        .subcommand(
            Command::new("purge", "remove a queue's jobs, or only those in one state")
                .arg(name())
                .flag(Flag::option("state", "STATE", "Only jobs in this state"))
                .flag(Flag::switch("yes", "Confirm; purging cannot be undone").short('y'))
                .example("belay queue purge emails --state dead --yes"),
        )
        .subcommand(
            Command::new("config", "show or change a queue's configuration")
                .details(
                    "Without options, prints the configuration as KEY = VALUE lines. --set takes\n\
                     the same keys (repeat it to change several); `none` removes an optional\n\
                     setting. --file replaces the whole configuration from JSON or TOML.",
                )
                .arg(name())
                .flag(
                    Flag::option("set", "KEY=VALUE", "Change one setting")
                        .repeated()
                        .conflicts_with("file"),
                )
                .flag(Flag::option("file", "PATH", "Replace the configuration from a file"))
                .example("belay queue config emails --set max_concurrency=8 --set retention.dead=30d")
                .example("belay queue config emails --set rate_limit.per_second=5 --set rate_limit.burst=10")
                .example("belay queue config emails --set rate_limit=none"),
        )
        .subcommand(
            Command::new("delete", "delete a queue and its configuration")
                .alias("rm")
                .arg(name())
                .flag(Flag::switch("yes", "Confirm; deleting cannot be undone").short('y')),
        )
}

pub(crate) fn run(ctx: &mut Ctx<'_>, m: &Matches, action: &str) -> Result<()> {
    if action == "list" {
        return list(ctx);
    }
    let name: QueueName = m.require("name")?;
    let client = ctx.client()?;
    let queues = client.queues();
    match action {
        "info" => {
            let info = queues.get(&name)?;
            if ctx.out.json {
                ctx.out.json(&info)?;
            } else {
                ctx.out.fields(&info_fields(&info))?;
            }
        }
        "pause" | "resume" => {
            if action == "pause" {
                queues.pause(&name)?;
            } else {
                queues.resume(&name)?;
            }
            let state = if action == "pause" {
                "paused"
            } else {
                "active"
            };
            if ctx.out.json {
                ctx.out
                    .json_value(&json!({"queue": name.as_str(), "state": state}))?;
            }
            ctx.out
                .done(format!("queue {name} is now {}", ctx.out.state(state)))?;
        }
        "purge" => {
            let state = m.get::<JobState>("state")?;
            if !m.flag("yes") {
                let what = state.map_or("every job".to_owned(), |s| format!("every {s} job"));
                return Err(m
                    .constraint(format!(
                        "refusing to purge {what} from {name} without --yes"
                    ))
                    .into());
            }
            let purged = queues.purge(&name, state)?;
            if ctx.out.json {
                ctx.out
                    .json_value(&json!({"queue": name.as_str(), "purged": purged}))?;
            } else if ctx.out.quiet {
                ctx.out.line(purged)?;
            } else {
                ctx.out.line(format!(
                    "purged {} from {name}",
                    belay_common::text::plural(purged, "job")
                ))?;
            }
        }
        "config" => config(ctx, m, &name)?,
        "delete" => {
            if !m.flag("yes") {
                return Err(m
                    .constraint(format!("refusing to delete queue {name} without --yes"))
                    .into());
            }
            queues.delete(&name)?;
            if ctx.out.json {
                ctx.out
                    .json_value(&json!({"queue": name.as_str(), "deleted": true}))?;
            }
            ctx.out.done(format!("deleted queue {name}"))?;
        }
        other => unreachable!("queue {other} is not declared"),
    }
    Ok(())
}

fn list(ctx: &mut Ctx<'_>) -> Result<()> {
    let queues: Vec<QueueInfo> = ctx
        .client()?
        .queues()
        .list_all()
        .collect::<std::result::Result<_, _>>()?;
    if ctx.out.json {
        return Ok(ctx
            .out
            .json_value(&Value::Array(queues.iter().map(ToJson::to_json).collect()))?);
    }
    if ctx.out.quiet {
        for q in &queues {
            ctx.out.line(&q.name)?;
        }
        return Ok(());
    }
    let mut grid = Grid::new(&[
        "QUEUE", "STATE", "READY", "SCHED", "RUNNING", "RETRY", "DEAD", "OLDEST", "DONE/S",
    ])
    .paint("STATE", state_style)
    .paint("DEAD", |n| (n != "0").then_some(crate::output::Style::Red));
    for header in [
        "READY", "SCHED", "RUNNING", "RETRY", "DEAD", "OLDEST", "DONE/S",
    ] {
        grid = grid.right(header);
    }
    for q in &queues {
        let c = &q.counts;
        grid.push([
            q.name.to_string(),
            if q.paused { "paused" } else { "active" }.to_owned(),
            c.ready.to_string(),
            c.scheduled.to_string(),
            c.running.to_string(),
            c.retrying.to_string(),
            c.dead.to_string(),
            q.oldest_ready_age
                .map(approx)
                .unwrap_or_else(|| "-".to_owned()),
            format!("{:.1}", q.throughput.succeeded_per_second()),
        ]);
    }
    Ok(ctx
        .out
        .grid(&grid, "no queues yet; they appear with their first job")?)
}

fn info_fields(info: &QueueInfo) -> Fields {
    let mut f = Fields::new();
    let c = &info.counts;
    let t = &info.throughput;
    f.add("name", &info.name)
        .state("state", if info.paused { "paused" } else { "active" })
        .add(
            "jobs",
            format!(
                "{} ready, {} scheduled, {} running, {} retrying\n{} succeeded, {} dead, {} cancelled",
                c.ready, c.scheduled, c.running, c.retrying, c.succeeded, c.dead, c.cancelled
            ),
        )
        .opt("oldest ready", info.oldest_ready_age.map(approx))
        .add(
            "throughput",
            format!(
                "last {}: {} enqueued, {} succeeded, {} failed, {} dead ({:.2}/s)",
                format_duration(t.window),
                t.enqueued,
                t.succeeded,
                t.failed,
                t.dead,
                t.succeeded_per_second()
            ),
        );
    for (key, value) in flatten(&info.config.to_json()) {
        f.add(&key, value);
    }
    f
}
