//! `cron`: [`CronExpr::next_after`] from random instants, for expressions
//! a scheduler typically holds, in UTC and in zones with daylight saving
//! time (New York and Berlin shift by an hour, Lord Howe Island by half an
//! hour). Each call is timed individually, so the latencies include one
//! `Instant::now()` (its cost is in the header).

use std::hint::black_box;

use belay_cron::{CronExpr, Tz};

use crate::harness::{measure, Params, Plan, Work};
use crate::session::{CaseResult, Session};
use crate::{Error, Result};

pub const NAME: &str = "cron";

/// Short names (used as the parameter) and expressions.
const EXPRESSIONS: &[(&str, &str)] = &[
    ("every-5m", "*/5 * * * *"),
    ("every-10s", "*/10 * * * * *"),
    ("weekdays-0930", "30 9 * * MON-FRI"),
    // 02:30 falls in the spring-forward gap of the US and EU zones.
    ("daily-0230", "30 2 * * *"),
    ("last-day", "0 0 L * *"),
    ("third-friday", "0 12 * * FRI#3"),
    // Matches once every four years: the search has to skip whole years.
    ("leap-day", "0 0 29 2 *"),
];

const ZONES: &[&str] = &[
    "UTC",
    "America/New_York",
    "Europe/Berlin",
    "Australia/Lord_Howe",
];

/// 2020-01-01T00:00:00Z and 2040-01-01T00:00:00Z: the window start
/// instants are drawn from.
const FROM: i64 = 1_577_836_800;
const UNTIL: i64 = 2_208_988_800;

pub fn run(s: &mut Session<'_>) -> Result<()> {
    let iterations = s.pick(200_000, 20_000, 3);
    let mut rng = s.rng(NAME);
    let starts: Vec<i64> = (0..4096).map(|_| rng.gen_range(FROM..UNTIL)).collect();
    for &(name, text) in EXPRESSIONS {
        for &zone in ZONES {
            let params = Params::new().with("expr", name).with("tz", zone);
            if !s.wants(NAME, &params) {
                continue;
            }
            let expr = CronExpr::parse(text)?;
            let tz = Tz::named(zone)?;
            let sample = measure(Plan::new(iterations), |seq| {
                let from = starts[seq as usize % starts.len()];
                match black_box(&expr).next_after(black_box(from), &tz) {
                    Some(next) if next > from => Ok(Work::ONE),
                    other => Err(Error::Invalid(format!(
                        "{text:?} in {zone} after {from} gave {other:?}"
                    ))),
                }
            })?;
            s.record(CaseResult::new(NAME, params, sample).note("cron", format!("{text:?}")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_expression_and_zone_is_valid() {
        for &(_, text) in EXPRESSIONS {
            let expr = CronExpr::parse(text).unwrap();
            for &zone in ZONES {
                let tz = Tz::named(zone).unwrap();
                // Early and late in the window, so no case can run past the
                // end of the supported range.
                for from in [FROM, UNTIL] {
                    assert!(expr.next_after(from, &tz).is_some(), "{text} in {zone}");
                }
            }
        }
    }
}
