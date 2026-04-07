//! `belay cron ...`: explaining and previewing cron expressions offline,
//! with the same parser and time zone rules the scheduler uses.

use belay_common::Timestamp;
use belay_cron::{CronExpr, Tz};
use belay_json::{json, Value};

use super::Ctx;
use crate::args::{Arg, Command, Flag, Matches};
use crate::error::{Error, Result};
use crate::output::human::relative;
use crate::output::Fields;

/// Fire times listed when `--count` is not given.
const DEFAULT_COUNT: usize = 5;
/// More would only be a slow way to fill a terminal.
const MAX_COUNT: usize = 1000;

pub(crate) fn spec() -> Command {
    let expr = || Arg::required("expr", "Cron expression, quoted: \"30 9 * * MON-FRI\"");
    let with_times = |c: Command| {
        c.arg(expr())
            .flag(
                Flag::option("tz", "ZONE", "Time zone: IANA name, +05:30 or a POSIX rule")
                    .default("UTC"),
            )
            .flag(
                Flag::option("count", "N", "How many fire times")
                    .short('n')
                    .default("5"),
            )
            .flag(Flag::option(
                "from",
                "TIME",
                "Start after this RFC 3339 time instead of now",
            ))
    };
    Command::new("cron", "explain and preview cron expressions (offline)")
        .subcommand(
            with_times(Command::new(
                "explain",
                "describe an expression and list its next fire times",
            ))
            .details(
                "Accepts 5 fields (minute hour day month weekday), 6 (with leading seconds)\n\
                     or 7 (with trailing year), names (JAN, MON), steps (*/15), L, W, # and\n\
                     nicknames such as @daily. When both day fields are restricted a day matches\n\
                     if either does, as in Vixie cron.",
            )
            .example("belay cron explain \"30 9 * * MON-FRI\" --tz America/New_York")
            .example("belay cron explain \"0 0 L * *\" -n 3"),
        )
        .subcommand(
            with_times(Command::new(
                "next",
                "print the next fire times, one per line",
            ))
            .example("belay cron next \"*/15 * * * *\" -n 4"),
        )
        .subcommand(
            Command::new("zones", "list the time zone names belay knows")
                .arg(Arg::optional("filter", "Only names containing this text")),
        )
}

pub(crate) fn run(ctx: &mut Ctx<'_>, m: &Matches, action: &str) -> Result<()> {
    if action == "zones" {
        return zones(ctx, m);
    }
    let text: String = m.require("expr")?;
    let expr = parse_expr(m, &text)?;
    let tz = parse_tz(m)?;
    let count: usize = m.get_or("count", DEFAULT_COUNT)?;
    if !(1..=MAX_COUNT).contains(&count) {
        return Err(m
            .constraint(format!(
                "--count must be between 1 and {MAX_COUNT}, got {count}"
            ))
            .into());
    }
    let from = m.get::<Timestamp>("from")?.unwrap_or_else(|| ctx.now());
    let fires: Vec<i64> = expr.upcoming(from.as_secs(), &tz).take(count).collect();
    match action {
        "explain" => explain(ctx, &text, &expr, &tz, from, &fires),
        "next" => next(ctx, &text, &tz, &fires),
        other => unreachable!("cron {other} is not declared"),
    }
}

/// Parses the expression; errors show the expression with a caret under
/// the column at fault.
fn parse_expr(m: &Matches, text: &str) -> Result<CronExpr> {
    CronExpr::parse(text).map_err(|e| {
        let mut message = format!("invalid cron expression: {e}");
        if let Some(column) = e.column() {
            let caret = " ".repeat(column) + "^";
            message.push_str(&format!("\n  {text}\n  {caret}"));
        }
        Error::Usage(m.constraint(message))
    })
}

fn parse_tz(m: &Matches) -> Result<Tz> {
    let name = m.value("tz").unwrap_or("UTC");
    Tz::parse(name).map_err(|e| {
        let usage = m.constraint(format!("invalid value {name:?} for --tz: {e}"));
        Error::Usage(usage.with_hint("list known zones with `belay cron zones`"))
    })
}

fn zone_label(tz: &Tz, at: i64) -> String {
    let offset = tz.format_rfc3339(at);
    // The offset is the RFC 3339 suffix: `Z`, or `+hh:mm` after the seconds.
    let offset = if offset.ends_with('Z') {
        "+00:00".to_owned()
    } else {
        offset[offset.len() - 6..].to_owned()
    };
    let name = tz.name().map_or_else(|| offset.clone(), str::to_owned);
    format!("{name} ({} {offset})", tz.abbreviation_at(at))
}

fn explain(
    ctx: &mut Ctx<'_>,
    text: &str,
    expr: &CronExpr,
    tz: &Tz,
    from: Timestamp,
    fires: &[i64],
) -> Result<()> {
    if ctx.out.json {
        let times: Vec<Value> = fires
            .iter()
            .map(|t| Value::String(tz.format_rfc3339(*t)))
            .collect();
        ctx.out.json_value(&json!({
            "expression": text,
            "normalized": expr.to_string(),
            "description": expr.describe(),
            "timezone": tz.name().map_or_else(|| text_offset(tz, from), str::to_owned),
            "next": times,
        }))?;
        return Ok(());
    }
    let mut f = Fields::new();
    f.add("expression", text);
    let normalized = expr.to_string();
    if normalized != text.trim() {
        f.add("normalized", normalized);
    }
    f.add("meaning", expr.describe())
        .add("time zone", zone_label(tz, from.as_secs()));
    if fires.is_empty() {
        f.add("next", "never: no future time matches");
    } else {
        let lines: Vec<String> = fires
            .iter()
            .map(|t| {
                let at = Timestamp::from_secs(*t);
                let local = tz.to_local(*t);
                format!(
                    "{}  {}  {}",
                    tz.format_rfc3339(*t),
                    &local.weekday().name()[..3],
                    relative(at, ctx.now())
                )
            })
            .collect();
        f.add(&format!("next {}", fires.len()), lines.join("\n"));
        if let Some(change) = tz.next_transition(from.as_secs()) {
            let last = fires[fires.len() - 1];
            if change.at <= last {
                let direction = if change.is_forward() {
                    "forward"
                } else {
                    "back"
                };
                f.add(
                    "note",
                    format!(
                        "clocks go {direction} at {} ({} to {}); fire times follow wall-clock time",
                        Timestamp::from_secs(change.at).to_rfc3339(),
                        offset_text(change.offset_before),
                        offset_text(change.offset_after),
                    ),
                );
            }
        }
    }
    Ok(ctx.out.fields(&f)?)
}

fn text_offset(tz: &Tz, at: Timestamp) -> String {
    offset_text(tz.offset_at(at.as_secs()))
}

fn offset_text(offset: i32) -> String {
    let sign = if offset < 0 { '-' } else { '+' };
    let abs = offset.unsigned_abs();
    format!("{sign}{:02}:{:02}", abs / 3600, abs % 3600 / 60)
}

fn next(ctx: &mut Ctx<'_>, text: &str, tz: &Tz, fires: &[i64]) -> Result<()> {
    if fires.is_empty() {
        return Err(Error::failed(format!(
            "{text:?} never fires after the start time"
        )));
    }
    if ctx.out.json {
        let times = fires
            .iter()
            .map(|t| Value::String(tz.format_rfc3339(*t)))
            .collect();
        return Ok(ctx.out.json_value(&Value::Array(times))?);
    }
    for t in fires {
        ctx.out.line(tz.format_rfc3339(*t))?;
    }
    Ok(())
}

fn zones(ctx: &mut Ctx<'_>, m: &Matches) -> Result<()> {
    let filter = m.value("filter").map(str::to_ascii_lowercase);
    let names: Vec<&str> = belay_cron::zone_names()
        .filter(|n| {
            filter
                .as_deref()
                .map_or(true, |f| n.to_ascii_lowercase().contains(f))
        })
        .collect();
    if ctx.out.json {
        let items = names
            .iter()
            .map(|n| Value::String((*n).to_owned()))
            .collect();
        return Ok(ctx.out.json_value(&Value::Array(items))?);
    }
    if names.is_empty() {
        return Ok(ctx
            .out
            .note("no zone matches; fixed offsets (+05:30) and POSIX rules work too")?);
    }
    for name in names {
        ctx.out.line(name)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_render_with_sign_and_minutes() {
        assert_eq!(offset_text(0), "+00:00");
        assert_eq!(offset_text(19_800), "+05:30");
        assert_eq!(offset_text(-18_000), "-05:00");
    }

    #[test]
    fn zone_labels_name_the_zone_and_current_offset() {
        let ny = Tz::named("America/New_York").unwrap();
        // 2025-01-15T12:00:00Z, standard time.
        assert_eq!(
            zone_label(&ny, 1_736_942_400),
            "America/New_York (EST -05:00)"
        );
        assert_eq!(zone_label(&Tz::Utc, 0), "UTC (UTC +00:00)");
        assert_eq!(
            zone_label(&Tz::fixed(19_800).unwrap(), 0),
            "+05:30 (+0530 +05:30)"
        );
    }
}
