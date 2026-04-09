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

#[derive(Debug)]
struct DaysArg(WeekdaySet);

impl FromArg for DaysArg {
    fn from_arg(text: &str) -> std::result::Result<DaysArg, String> {
        text.parse::<WeekdaySet>()
            .map(DaysArg)
            .map_err(|e| e.to_string())
    }
}

#[derive(Debug)]
struct DateArg(CalendarDate);

impl FromArg for DateArg {
    fn from_arg(text: &str) -> std::result::Result<DateArg, String> {
        text.parse::<CalendarDate>()
            .map(DateArg)
            .map_err(|e| e.to_string())
    }
}

fn windows_text(c: &CalendarSpec) -> String {
    if c.windows.is_empty() {
        return "all day".to_owned();
    }
    let windows: Vec<String> = c
        .windows
        .iter()
        .map(|w| {
            if w.days.is_empty() {
                format!("{}-{}", w.start, w.end)
            } else {
                format!("{} {}-{}", w.days, w.start, w.end)
            }
        })
        .collect();
    windows.join("; ")
}

fn excludes_text(c: &CalendarSpec) -> String {
    let mut parts = Vec::new();
    if !c.exclude_weekdays.is_empty() {
        parts.push(c.exclude_weekdays.to_string());
    }
    match c.exclude_dates.len() {
        0 => {}
        n if n <= 3 => parts.extend(c.exclude_dates.iter().map(ToString::to_string)),
        n => parts.push(belay_common::text::plural(n as u64, "date")),
    }
    if parts.is_empty() {
        "-".to_owned()
    } else {
        parts.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_parse_with_or_without_days() {
        let w = WindowArg::from_arg("mon-fri 09:00-17:00").unwrap().0;
        assert_eq!(w.days.len(), 5);
        assert_eq!(
            (w.start.to_string(), w.end.to_string()),
            ("09:00".into(), "17:00".into())
        );
        let w = WindowArg::from_arg("22:00-06:00").unwrap().0;
        assert!(w.days.is_empty());
        assert!(WindowArg::from_arg("mon-fri")
            .unwrap_err()
            .contains("HH:MM-HH:MM"));
        assert!(WindowArg::from_arg("mon-fri 0900").is_err());
        assert!(WindowArg::from_arg("a b c").is_err());
        assert!(WindowArg::from_arg("25:00-26:00").is_err());
    }

    #[test]
    fn summaries_stay_short() {
        let mut c = CalendarSpec::new("x");
        assert_eq!(windows_text(&c), "all day");
        assert_eq!(excludes_text(&c), "-");
        c.windows
            .push(WindowArg::from_arg("sat,sun 10:00-12:00").unwrap().0);
        c.windows
            .push(WindowArg::from_arg("08:00-09:00").unwrap().0);
        assert_eq!(windows_text(&c), "sat,sun 10:00-12:00; 08:00-09:00");
        for d in ["2025-12-24", "2025-12-25", "2025-12-26", "2025-12-31"] {
            c.exclude_dates.insert(d.parse().unwrap());
        }
        c.exclude_weekdays = "sun".parse().unwrap();
        assert_eq!(excludes_text(&c), "sun, 4 dates");
    }
}
