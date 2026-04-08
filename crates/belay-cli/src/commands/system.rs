//! Server-wide read-outs: `info`, `version`, `members` and `workers`.

use std::time::Duration;

use belay_json::{json, ToJson, Value};

use super::Ctx;
use crate::args::Command;
use crate::error::Result;
use crate::output::human::{approx, relative, when};
use crate::output::{state_style, Fields, Grid, Style};

/// Clock differences below this are not worth mentioning: they are within
/// the time the request itself took.
const SKEW_TOLERANCE: Duration = Duration::from_secs(2);

pub(crate) fn info_spec() -> Command {
    Command::new(
        "info",
        "show the server's version, node, storage and uptime",
    )
}

pub(crate) fn version_spec() -> Command {
    Command::new("version", "print the version of this belay command")
}

pub(crate) fn members_spec() -> Command {
    Command::new(
        "members",
        "list the cluster's members as the server sees them",
    )
}

pub(crate) fn workers_spec() -> Command {
    Command::new(
        "workers",
        "list the workers the server has heard from recently",
    )
}

pub(crate) fn version(ctx: &mut Ctx<'_>) -> Result<()> {
    let version = env!("CARGO_PKG_VERSION");
    if ctx.out.json {
        ctx.out.json_value(&json!({"version": version}))?;
    } else if ctx.out.quiet {
        ctx.out.line(version)?;
    } else {
        ctx.out.line(format!("belay {version}"))?;
    }
    Ok(())
}

pub(crate) fn info(ctx: &mut Ctx<'_>) -> Result<()> {
    let client = ctx.client()?;
    let info = client.info()?;
    let ready = client.ready()?;
    if ctx.out.json {
        let mut value = info.to_json();
        if let Value::Object(map) = &mut value {
            map.insert("ready", ready);
        }
        return Ok(ctx.out.json_value(&value)?);
    }
    let now = ctx.now();
    let skew = match info.time.duration_since(now) {
        Some(ahead) if ahead > SKEW_TOLERANCE => Some(format!(
            "server clock is {} ahead of this machine",
            approx(ahead)
        )),
        _ => match now.duration_since(info.time) {
            Some(behind) if behind > SKEW_TOLERANCE => Some(format!(
                "server clock is {} behind this machine",
                approx(behind)
            )),
            _ => None,
        },
    };
    let mut f = Fields::new();
    f.add("server", &ctx.url)
        .add("version", &info.version)
        .add("node", &info.node)
        .add("storage", &info.storage)
        .add("started", when(info.started_at, now))
        .add("uptime", approx(info.uptime))
        .add(
            "cluster",
            belay_common::text::plural(u64::from(info.cluster_size), "live node"),
        )
        .add(
            "ready",
            if ready {
                "yes"
            } else {
                "no (starting, joining its cluster or shutting down)"
            },
        );
    if let Some(skew) = skew {
        f.add("clock", ctx.out.paint(&skew, Style::Yellow));
    }
    if info.version != env!("CARGO_PKG_VERSION") {
        f.add(
            "cli",
            format!("{} (differs from the server)", env!("CARGO_PKG_VERSION")),
        );
    }
    Ok(ctx.out.fields(&f)?)
}

pub(crate) fn members(ctx: &mut Ctx<'_>) -> Result<()> {
    let members = ctx.client()?.members()?;
    if ctx.out.json {
        let items = members.iter().map(ToJson::to_json).collect();
        return Ok(ctx.out.json_value(&Value::Array(items))?);
    }
    if ctx.out.quiet {
        for member in &members {
            ctx.out.line(&member.id)?;
        }
        return Ok(());
    }
    let mut grid = Grid::new(&["NODE", "ADDRESS", "STATE", "LEADER"]).paint("STATE", state_style);
    for member in &members {
        grid.push([
            member.id.clone(),
            member.addr.clone(),
            member.state.to_string(),
            if member.leader { "yes" } else { "" }.to_owned(),
        ]);
    }
    Ok(ctx.out.grid(&grid, "no members")?)
}

pub(crate) fn workers(ctx: &mut Ctx<'_>) -> Result<()> {
    let workers = ctx.client()?.workers()?;
    if ctx.out.json {
        let items = workers.iter().map(ToJson::to_json).collect();
        return Ok(ctx.out.json_value(&Value::Array(items))?);
    }
    if ctx.out.quiet {
        for worker in &workers {
            ctx.out.line(&worker.id)?;
        }
        return Ok(());
    }
    let now = ctx.now();
    let mut grid = Grid::new(&[
        "WORKER",
        "QUEUES",
        "ACTIVE",
        "LEASED",
        "SUCCEEDED",
        "FAILED",
        "LAST SEEN",
        "ADDRESS",
    ])
    .max_width("QUEUES", 40);
    for header in ["ACTIVE", "LEASED", "SUCCEEDED", "FAILED"] {
        grid = grid.right(header);
    }
    for w in &workers {
        let queues: Vec<&str> = w.queues.iter().map(|q| q.as_str()).collect();
        grid.push([
            w.id.to_string(),
            queues.join(","),
            w.active_jobs.to_string(),
            w.leased.to_string(),
            w.succeeded.to_string(),
            w.failed.to_string(),
            relative(w.last_seen, now),
            w.addr.clone().unwrap_or_default(),
        ]);
    }
    Ok(ctx.out.grid(&grid, "no workers seen recently")?)
}
