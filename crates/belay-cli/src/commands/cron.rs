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
