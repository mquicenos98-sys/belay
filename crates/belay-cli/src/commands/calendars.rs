//! `belay calendar ...`: named rules restricting when schedules may fire.

use belay_json::{json, ToJson, Value};
use belay_proto::schedule::{CalendarDate, TimeOfDay, TimeWindow, WeekdaySet};
use belay_proto::{CalendarSpec, Validate};

use super::Ctx;
use crate::args::{Arg, Command, Flag, FromArg, Matches};
use crate::error::Result;
use crate::input;
use crate::output::{Fields, Grid};

pub(crate) fn spec() -> Command {
    let name = || Arg::required("name", "Calendar name");
    Command::new("calendar", "manage calendars that restrict when schedules fire")
        .subcommand(Command::new("list", "list calendars").alias("ls"))
        .subcommand(Command::new("get", "show a calendar").alias("show").arg(name()))
        .subcommand(
            Command::new("put", "create or replace a calendar from flags or a file")
                .details(
                    "A time is allowed when its date and weekday are not excluded and it falls\n\
                     inside one of the windows (or there are none). Windows are local times in\n\
                     the calendar's zone; one whose end is not after its start runs past midnight.",
                )
                .arg(Arg::optional("name", "Calendar name (inline form)"))
                .flag(Flag::option("file", "PATH", "Load the spec from a JSON or TOML file"))
                .flag(Flag::option("tz", "ZONE", "Time zone of the rules").default("UTC"))
                .flag(
                    Flag::option("window", "[DAYS] HH:MM-HH:MM", "An allowed time range")
                        .repeated(),
                )
                .flag(
                    Flag::option("exclude-date", "YYYY-MM-DD", "A date on which nothing fires")
                        .repeated(),
                )
                .flag(Flag::option("exclude-weekdays", "DAYS", "Weekdays on which nothing fires"))
                .flag(Flag::option("description", "TEXT", "Free text for humans"))
                .example("belay calendar put business-hours --tz Europe/Berlin --window 'mon-fri 09:00-17:00' --exclude-date 2025-12-25")
                .example("belay calendar put --file calendars/holidays.toml"),
        )
        .subcommand(Command::new("rm", "delete a calendar").alias("delete").arg(name()))
}

pub(crate) fn run(ctx: &mut Ctx<'_>, m: &Matches, action: &str) -> Result<()> {
    let client = ctx.client()?;
    let calendars = client.calendars();
    match action {
        "list" => {
            let all: Vec<CalendarSpec> = calendars
                .list_all()
                .collect::<std::result::Result<_, _>>()?;
            if ctx.out.json {
                let items = all.iter().map(ToJson::to_json).collect();
                return Ok(ctx.out.json_value(&Value::Array(items))?);
            }
            if ctx.out.quiet {
                for c in &all {
                    ctx.out.line(&c.name)?;
                }
                return Ok(());
            }
            let mut grid = Grid::new(&["NAME", "TZ", "WINDOWS", "EXCLUDES", "DESCRIPTION"])
                .max_width("WINDOWS", 40)
                .max_width("DESCRIPTION", 40);
            for c in &all {
                grid.push([
                    c.name.clone(),
                    c.timezone.clone(),
                    windows_text(c),
                    excludes_text(c),
                    c.description.clone().unwrap_or_default(),
                ]);
            }
            ctx.out.grid(&grid, "no calendars")?;
        }
        "get" => {
            let name: String = m.require("name")?;
            let calendar = calendars.get(&name)?;
            if ctx.out.json {
                ctx.out.json(&calendar)?;
            } else {
                let mut f = Fields::new();
                f.add("name", &calendar.name)
                    .add("time zone", &calendar.timezone)
                    .add("windows", windows_text(&calendar))
                    .add("excludes", excludes_text(&calendar))
                    .opt("description", calendar.description.as_ref());
                ctx.out.fields(&f)?;
            }
        }
        "put" => {
            let spec = match m.value("file") {
                Some(path) => {
                    let conflicting = [
                        "name",
                        "window",
                        "exclude-date",
                        "exclude-weekdays",
                        "description",
                        "tz",
                    ];
                    if let Some(flag) = conflicting.iter().find(|f| m.given(f)) {
                        return Err(m
                            .constraint(format!("--file cannot be combined with {}", display(flag)))
                            .into());
                    }
                    input::spec_file::<CalendarSpec>(ctx, path)?
                }
                None => inline(m)?,
            };
            let saved = calendars.put(&spec)?;
            if ctx.out.json {
                ctx.out.json(&saved)?;
            } else if ctx.out.quiet {
                ctx.out.line(&saved.name)?;
            } else {
                ctx.out.line(format!("saved calendar {}", saved.name))?;
            }
        }
        "rm" => {
            let name: String = m.require("name")?;
            calendars.delete(&name)?;
            if ctx.out.json {
                ctx.out
                    .json_value(&json!({"calendar": name.as_str(), "deleted": true}))?;
            }
            ctx.out.done(format!("deleted calendar {name}"))?;
        }
        other => unreachable!("calendar {other} is not declared"),
    }
    Ok(())
}

fn display(flag: &str) -> String {
    if flag == "name" {
        "<NAME>".to_owned()
    } else {
        format!("--{flag}")
    }
}

fn inline(m: &Matches) -> Result<CalendarSpec> {
    let Some(name) = m.get::<String>("name")? else {
        return Err(m
            .constraint("give the calendar's <NAME>, or load a spec with --file")
            .into());
    };
    let mut spec = CalendarSpec::new(name);
    spec.timezone = m.value("tz").unwrap_or("UTC").to_owned();
    spec.windows = m
        .get_all::<WindowArg>("window")?
        .into_iter()
        .map(|w| w.0)
        .collect();
    spec.exclude_dates = m
        .get_all::<DateArg>("exclude-date")?
        .into_iter()
        .map(|d| d.0)
        .collect();
    if let Some(days) = m.get::<DaysArg>("exclude-weekdays")? {
        spec.exclude_weekdays = days.0;
    }
    spec.description = m.get("description")?;
    spec.validate()
        .map_err(|e| m.constraint(format!("invalid calendar: {e}")))?;
    belay_cron::Tz::parse(&spec.timezone)
        .map_err(|e| m.constraint(format!("invalid value for --tz: {e}")))?;
    Ok(spec)
}

/// `mon-fri 09:00-17:00` or just `09:00-17:00` (every day).
#[derive(Debug)]
struct WindowArg(TimeWindow);

impl FromArg for WindowArg {
    fn from_arg(text: &str) -> std::result::Result<WindowArg, String> {
        const SHAPE: &str = "expected [DAYS] HH:MM-HH:MM, e.g. 'mon-fri 09:00-17:00'";
        let parts: Vec<&str> = text.split_whitespace().collect();
        let (days, range) = match parts.as_slice() {
            [range] => (WeekdaySet::EMPTY, *range),
            [days, range] => (DaysArg::from_arg(days)?.0, *range),
            _ => return Err(SHAPE.to_owned()),
        };
        let Some((start, end)) = range.split_once('-').filter(|_| range.contains(':')) else {
            return Err(SHAPE.to_owned());
        };
        let time = |t: &str| t.parse::<TimeOfDay>().map_err(|e| e.to_string());
        Ok(WindowArg(TimeWindow::new(days, time(start)?, time(end)?)))
    }
}
