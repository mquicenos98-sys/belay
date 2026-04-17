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

/// `queue config`: print, `--set`, or `--file`.
fn config(ctx: &mut Ctx<'_>, m: &Matches, name: &QueueName) -> Result<()> {
    let client = ctx.client()?;
    let sets: Vec<KeyValue> = m.get_all("set")?;
    let new_config = if let Some(path) = m.value("file") {
        input::spec_file::<QueueConfig>(ctx, path)?
    } else if !sets.is_empty() {
        let mut value = client.queues().get(name)?.config.to_json();
        for set in &sets {
            input::set_path(&mut value, &set.key, &set.value)
                .map_err(|e| m.constraint(format!("invalid --set {}: {e}", set.key)))?;
        }
        input::decode::<QueueConfig>(&Origin::Inline, &value).map_err(|e| {
            m.constraint(format!(
                "invalid --set: {}",
                e.to_string().trim_start_matches("the command line: ")
            ))
        })?
    } else {
        let config = client.queues().get(name)?.config;
        return print_config(ctx, &config);
    };
    let info = client.queues().set_config(name, &new_config)?;
    if !ctx.out.json {
        ctx.out.done(format!("updated queue {name}"))?;
    }
    if ctx.out.quiet {
        return Ok(());
    }
    print_config(ctx, &info.config)
}

fn print_config(ctx: &mut Ctx<'_>, config: &QueueConfig) -> Result<()> {
    if ctx.out.json {
        return Ok(ctx.out.json(config)?);
    }
    for (key, value) in flatten(&config.to_json()) {
        ctx.out.line(format!("{key} = {value}"))?;
    }
    Ok(())
}

/// Dotted keys and scalar values of a JSON object, in order:
/// `retention.dead`, `7d`. Unset optional settings (absent keys) are shown
/// as `none` for the two that exist, so users see what they can set.
fn flatten(value: &Value) -> Vec<(String, String)> {
    fn walk(prefix: &str, value: &Value, out: &mut Vec<(String, String)>) {
        match value {
            Value::Object(map) => {
                for (key, child) in map.iter() {
                    let path = if prefix.is_empty() {
                        key.to_owned()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    walk(&path, child, out);
                }
            }
            Value::String(s) => out.push((prefix.to_owned(), s.clone())),
            other => out.push((prefix.to_owned(), other.to_string())),
        }
    }
    // The optional settings come first in a configuration's JSON; keep
    // that order whether or not they are set.
    const OPTIONAL: [&str; 2] = ["max_concurrency", "rate_limit"];
    let mut out = Vec::new();
    for key in OPTIONAL {
        match value.get(key) {
            Some(child) => walk(key, child, &mut out),
            None => out.push((key.to_owned(), "none".to_owned())),
        }
    }
    if let Value::Object(map) = value {
        for (key, child) in map.iter().filter(|(k, _)| !OPTIONAL.contains(k)) {
            walk(key, child, &mut out);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_proto::RateLimit;

    #[test]
    fn configs_flatten_to_the_keys_set_accepts() {
        let config = QueueConfig {
            rate_limit: Some(RateLimit::new(2.5, 5)),
            ..QueueConfig::default()
        };
        let flat = flatten(&config.to_json());
        let keys: Vec<&str> = flat.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "max_concurrency",
                "rate_limit.per_second",
                "rate_limit.burst",
                "default_timeout",
                "default_retry",
                "default_max_attempts",
                "retention.succeeded",
                "retention.dead",
                "retention.cancelled"
            ]
        );
        assert_eq!(flat[0].1, "none");
        assert_eq!(flat[1].1, "2.5");
        assert_eq!(flat[4].1, "exp:1s*2..1h~10%");
    }

    #[test]
    fn every_flattened_key_can_be_set_back() {
        let mut value = QueueConfig::default().to_json();
        for (key, raw) in flatten(&value.clone()) {
            input::set_path(&mut value, &key, &raw).unwrap();
        }
        let back = input::decode::<QueueConfig>(&Origin::Inline, &value).unwrap();
        assert_eq!(back, QueueConfig::default());
    }
}
