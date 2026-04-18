//! `belay workflow ...`: submitting DAGs of jobs and following their runs.

use std::time::{Duration, Instant};

use belay_client::ListOptions;
use belay_common::Timestamp;
use belay_json::{json, ToJson, Value};
use belay_proto::workflow::WorkflowState;
use belay_proto::{Workflow, WorkflowId, WorkflowSpec};

use super::Ctx;
use crate::args::{Arg, Command, Flag, Matches};
use crate::error::{Error, Result};
use crate::input;
use crate::output::human::{approx, relative, when};
use crate::output::{state_style, Fields, Grid};

pub(crate) fn spec() -> Command {
    let id = || Arg::required("id", "Workflow run id");
    Command::new("workflow", "run and follow workflows (DAGs of jobs)")
        .subcommand(
            Command::new("submit", "start a workflow run from a JSON or TOML spec")
                .details(
                    "The spec has a name and steps; each step names a job and the steps it\n\
                     depends_on. The server checks the graph (unique names, known dependencies,\n\
                     no cycles) and enqueues the steps without dependencies at once.",
                )
                .arg(Arg::required("file", "Spec file, or - for stdin"))
                .flag(Flag::switch("wait", "Wait for the run to finish").short('w'))
                .flag(Flag::option(
                    "wait-timeout",
                    "DURATION",
                    "Stop waiting after this long",
                ))
                .example("belay workflow submit etl.json --wait"),
        )
        .subcommand(
            Command::new("get", "show a run and its steps")
                .alias("show")
                .arg(id()),
        )
        .subcommand(
            Command::new("list", "list workflow runs")
                .alias("ls")
                .flag(Flag::option("limit", "N", "Runs per page").short('n'))
                .flag(Flag::switch("all", "Follow every page to the end")),
        )
        .subcommand(Command::new("cancel", "cancel a run and its unfinished steps").arg(id()))
}

pub(crate) fn run(ctx: &mut Ctx<'_>, m: &Matches, action: &str) -> Result<()> {
    let client = ctx.client()?;
    let workflows = client.workflows();
    match action {
        "submit" => {
            let path: String = m.require("file")?;
            if m.given("wait-timeout") && !m.flag("wait") {
                return Err(m.constraint("--wait-timeout needs --wait").into());
            }
            let spec: WorkflowSpec = input::spec_file(ctx, &path)?;
            let run = workflows.submit(&spec)?;
            if m.flag("wait") {
                let timeout = m.get::<Duration>("wait-timeout")?;
                let run = wait(ctx, &client, run.id, timeout)?;
                return finished(ctx, &run);
            }
            if ctx.out.json {
                ctx.out.json(&run)?;
            } else if ctx.out.quiet {
                ctx.out.line(run.id)?;
            } else {
                ctx.out.line(format!(
                    "started workflow {} ({}, {})",
                    run.id,
                    run.spec.name,
                    belay_common::text::plural(run.spec.steps.len() as u64, "step")
                ))?;
            }
        }
        "get" => {
            let id: WorkflowId = m.require("id")?;
            let run = workflows.get(id)?;
            if ctx.out.json {
                ctx.out.json(&run)?;
            } else {
                show(ctx, &run)?;
            }
        }
        "list" => {
            let runs: Vec<Workflow> = if m.flag("all") {
                workflows
                    .list_all()
                    .collect::<std::result::Result<_, _>>()?
            } else {
                let mut options = ListOptions::new();
                if let Some(limit) = m.get::<u32>("limit")? {
                    options = options.limit(limit);
                }
                workflows.list(&options)?.items
            };
            if ctx.out.json {
                let items = runs.iter().map(ToJson::to_json).collect();
                return Ok(ctx.out.json_value(&Value::Array(items))?);
            }
            if ctx.out.quiet {
                for r in &runs {
                    ctx.out.line(r.id)?;
                }
                return Ok(());
            }
            let now = ctx.now();
            let mut grid = Grid::new(&["ID", "NAME", "STATE", "STEPS", "CREATED", "TOOK"])
                .paint("STATE", state_style);
            for r in &runs {
                let done = r.steps.values().filter(|s| s.state.is_finished()).count();
                grid.push([
                    r.id.to_string(),
                    r.spec.name.clone(),
                    r.state.to_string(),
                    format!("{done}/{}", r.steps.len()),
                    relative(r.created_at, now),
                    r.finished_at
                        .map(|f| approx(f.saturating_duration_since(r.created_at)))
                        .unwrap_or_else(|| "-".to_owned()),
                ]);
            }
            ctx.out.grid(&grid, "no workflow runs")?;
        }
        "cancel" => {
            let id: WorkflowId = m.require("id")?;
            workflows.cancel(id)?;
            if ctx.out.json {
                ctx.out
                    .json_value(&json!({"workflow": id.to_string(), "cancelled": true}))?;
            }
            ctx.out.done(format!("cancelled workflow {id}"))?;
        }
        other => unreachable!("workflow {other} is not declared"),
    }
    Ok(())
}

/// The run's summary and a table of its steps in declaration order.
fn show(ctx: &mut Ctx<'_>, run: &Workflow) -> Result<()> {
    let now = ctx.now();
    let mut f = Fields::new();
    f.add("id", run.id)
        .add("name", &run.spec.name)
        .state("state", run.state.as_str())
        .add("created", when(run.created_at, now))
        .opt("finished", run.finished_at.map(|t| when(t, now)));
    ctx.out.fields(&f)?;
    ctx.out.line("")?;
    ctx.out.grid(&steps_grid(run, now), "no steps")?;
    Ok(())
}

fn steps_grid(run: &Workflow, now: Timestamp) -> Grid {
    let mut grid = Grid::new(&["STEP", "STATE", "JOB", "AFTER", "STARTED", "TOOK"])
        .paint("STATE", state_style);
    for step in &run.spec.steps {
        let Some(status) = run.steps.get(&step.name) else {
            continue;
        };
        grid.push([
            step.name.clone(),
            status.state.to_string(),
            status
                .job
                .map(|j| j.to_string())
                .unwrap_or_else(|| "-".to_owned()),
            if step.depends_on.is_empty() {
                "-".to_owned()
            } else {
                step.depends_on.join(",")
            },
            status
                .started_at
                .map(|t| relative(t, now))
                .unwrap_or_else(|| "-".to_owned()),
            match (status.started_at, status.finished_at) {
                (Some(s), Some(f)) => approx(f.saturating_duration_since(s)),
                _ => "-".to_owned(),
            },
        ]);
    }
    grid
}

fn wait(
    ctx: &mut Ctx<'_>,
    client: &belay_client::Client,
    id: WorkflowId,
    timeout: Option<Duration>,
) -> Result<Workflow> {
    let started = Instant::now();
    let mut pause = Duration::from_millis(50);
    loop {
        let run = client.workflows().get(id)?;
        if run.state.is_terminal() {
            return Ok(run);
        }
        if timeout.is_some_and(|limit| started.elapsed() >= limit) {
            return Err(Error::Failed {
                message: format!("workflow {id} is still running"),
                hint: Some(format!("follow it with `belay workflow get {id}`")),
            });
        }
        ctx.pause(pause);
        pause = (pause * 2).min(Duration::from_millis(500));
    }
}

fn finished(ctx: &mut Ctx<'_>, run: &Workflow) -> Result<()> {
    if ctx.out.json {
        ctx.out.json(run)?;
    } else if ctx.out.quiet {
        ctx.out.line(run.state)?;
    } else {
        show(ctx, run)?;
    }
    match run.state {
        WorkflowState::Succeeded => Ok(()),
        other => Err(Error::Failed {
            message: format!("workflow {} {other}", run.id),
            hint: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_proto::workflow::{StepSpec, StepState};
    use belay_proto::JobSpec;

    #[test]
    fn steps_are_listed_in_declaration_order_with_dependencies() {
        let spec = WorkflowSpec::new("etl")
            .step(StepSpec::new("extract", JobSpec::new("etl", "extract")))
            .step(StepSpec::new("load", JobSpec::new("etl", "load")).after("extract"));
        let now = Timestamp::from_secs(1_700_000_000);
        let mut run = Workflow::new("01HQ3K9Z8N6V2T4R5S7W0X1Y2Z".parse().unwrap(), spec, now);
        if let Some(step) = run.steps.get_mut("extract") {
            step.state = StepState::Succeeded;
            step.started_at = Some(now);
            step.finished_at = Some(now + Duration::from_secs(3));
        }
        let text = steps_grid(&run, now).render(false);
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines[1].starts_with("extract  succeeded  -    -        just now  3.0s"),
            "{text}"
        );
        assert!(
            lines[2].starts_with("load     pending    -    extract"),
            "{text}"
        );
    }
}
