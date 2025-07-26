//! Plain-English descriptions of expressions.
//!
//! The description is built from the field sets, not the original text, so
//! it describes what the expression *does*: `0,30 * * * *` reads "every 30
//! minutes" just like `*/30 * * * *`. Common shapes get idiomatic phrasing
//! ("every hour", "at 09:30 on Monday through Friday"); unusual combinations
//! fall back to a mechanical but complete description, never a wrong one.

use crate::civil::Weekday;
use crate::expr::{bits, CronExpr, DomSpec, DowSpec};
use crate::format::{progression, runs, weekday_runs};
use crate::DayMatch;

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// The rough shape of a field's value set.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Shape {
    All,
    Single(u32),
    /// Arithmetic progression `start, start + step, ..., end`.
    Step {
        start: u32,
        step: u32,
        end: u32,
    },
    Range(u32, u32),
    List(Vec<u32>),
}

impl Shape {
    fn of(values: &[u32], min: u32, max: u32) -> Shape {
        if values.len() as u32 == max - min + 1 {
            return Shape::All;
        }
        if let [only] = values {
            return Shape::Single(*only);
        }
        if let Some(step) = progression(values, min, max) {
            return Shape::Step {
                start: values[0],
                step,
                end: values[values.len() - 1],
            };
        }
        let first = values[0];
        let last = values[values.len() - 1];
        if last - first + 1 == values.len() as u32 {
            return Shape::Range(first, last);
        }
        Shape::List(values.to_vec())
    }
}

impl CronExpr {
    /// An English description of when the expression fires.
    ///
    /// ```
    /// use belay_cron::CronExpr;
    ///
    /// let d = |s: &str| CronExpr::parse(s).unwrap().describe();
    /// assert_eq!(d("30 9 * * 1-5"), "at 09:30 on Monday through Friday");
    /// assert_eq!(d("*/15 * * * *"), "every 15 minutes");
    /// assert_eq!(d("0 0 L * *"), "at 00:00 on the last day of the month");
    /// assert_eq!(d("0 0 1 1 *"), "at 00:00 on January 1");
    /// ```
    #[must_use]
    pub fn describe(&self) -> String {
        let (time, exact) = describe_time(self);
        let mut parts = vec![time];
        let months: Vec<u32> = bits(u64::from(self.months)).collect();
        let mut months_done = false;
        match describe_days(self, &months) {
            Some((days, used_months)) => {
                parts.push(days);
                months_done = used_months;
            }
            None if exact => parts.push("every day".to_string()),
            None => {}
        }
        if !months_done && months.len() < 12 {
            parts.push(format!("in {}", month_list(&months)));
        }
        if self.has_year() {
            let years: Vec<u32> = self.years.iter().collect();
            parts.push(describe_years(&years));
        }
        parts.join(" ")
    }
}

/// The time-of-day phrase, and whether it names exact clock times.
fn describe_time(e: &CronExpr) -> (String, bool) {
    let secs: Vec<u32> = bits(e.seconds).collect();
    let mins: Vec<u32> = bits(e.minutes).collect();
    let hours: Vec<u32> = bits(u64::from(e.hours)).collect();
    let s = Shape::of(&secs, 0, 59);
    let m = Shape::of(&mins, 0, 59);
    let h = Shape::of(&hours, 0, 23);

    // A handful of exact clock times, unless they are simply "every N hours"
    // with N dividing the day.
    if let (Shape::Single(sec), Shape::Single(min)) = (&s, &m) {
        let even_hours =
            matches!(h, Shape::Step { step, end, .. } if 24 % step == 0 && end + step > 23);
        let on_the_hour = *sec == 0 && *min == 0;
        if h != Shape::All && hours.len() <= 6 && !(on_the_hour && even_hours) {
            let times: Vec<String> = hours.iter().map(|&hr| clock(hr, *min, *sec)).collect();
            return (format!("at {}", join_and(&times)), true);
        }
    }

    let phrase = if let Shape::Single(sec) = s {
        match m {
            Shape::Single(min) => minute_of_hours(min, sec, &h, &hours),
            _ => {
                let mut p = minutes_phrase(&m);
                if let Some(q) = hours_qualifier(&h, &hours) {
                    p = format!("{p} {q}");
                } else if matches!(m, Shape::Range(..) | Shape::List(_)) {
                    p.push_str(" past every hour");
                }
                if sec != 0 {
                    p = format!("{p}, at second {sec}");
                }
                p
            }
        }
    } else {
        let mut p = seconds_phrase(&s);
        match (&m, &h) {
            (Shape::All, Shape::All) => {}
            (Shape::Single(min), Shape::Single(hr)) => {
                p = format!(
                    "{p} between {} and {}",
                    clock(*hr, *min, 0),
                    clock(*hr, *min, 59)
                );
            }
            (Shape::All, _) => {
                if let Some(q) = hours_qualifier(&h, &hours) {
                    p = format!("{p} {q}");
                }
            }
            _ => {
                p = format!("{p} during {}", minutes_phrase(&m));
                if let Some(q) = hours_qualifier(&h, &hours) {
                    p = format!("{p} {q}");
                }
            }
        }
        p
    };
    (phrase, false)
}

/// A single minute (and second) in several or all hours.
fn minute_of_hours(min: u32, sec: u32, h: &Shape, hours: &[u32]) -> String {
    let on_the_hour = min == 0 && sec == 0;
    let past = if sec == 0 {
        format!("at {min} {} past", plural(min, "minute"))
    } else {
        format!("at {min:02}:{sec:02} past")
    };
    match h {
        Shape::All if on_the_hour => "every hour".to_string(),
        Shape::All => format!("{past} every hour"),
        Shape::Step { start, step, end } if end + step > 23 => {
            let every = if on_the_hour {
                every(*step, "hour", 24)
            } else {
                format!("{past} every {} hour", ordinal(*step))
            };
            if *start == 0 {
                every
            } else {
                format!("{every} starting at {}", clock(*start, 0, 0))
            }
        }
        Shape::Range(a, b) => {
            let lead = if on_the_hour {
                "every hour".to_string()
            } else {
                format!("{past} every hour")
            };
            format!(
                "{lead} between {} and {}",
                clock(*a, 0, 0),
                clock(*b, 59, 0)
            )
        }
        _ => {
            let list: Vec<String> = hours.iter().map(|h| h.to_string()).collect();
            if on_the_hour {
                format!("at the start of hours {}", join_and(&list))
            } else {
                format!("{past} hours {}", join_and(&list))
            }
        }
    }
}

fn seconds_phrase(s: &Shape) -> String {
    unit_phrase(s, "second", 60)
}

fn minutes_phrase(m: &Shape) -> String {
    unit_phrase(m, "minute", 60)
}

/// "every 15 minutes", "at minutes 0, 20 and 45", "at minutes 10 through 20".
fn unit_phrase(shape: &Shape, unit: &str, cycle: u32) -> String {
    match shape {
        Shape::All => format!("every {unit}"),
        Shape::Single(v) => format!("at {unit} {v}"),
        Shape::Step { start, step, end } => {
            let reaches_end = end + step >= cycle;
            match (*start == 0, reaches_end) {
                (true, true) => every(*step, unit, cycle),
                (false, true) => {
                    format!("{} starting at {unit} {start}", every(*step, unit, cycle))
                }
                _ => format!(
                    "every {} {unit} from {unit} {start} through {end}",
                    ordinal(*step)
                ),
            }
        }
        Shape::Range(a, b) => format!("at {unit}s {a} through {b}"),
        Shape::List(values) => {
            let items: Vec<String> = values.iter().map(u32::to_string).collect();
            format!("at {unit}s {}", join_and(&items))
        }
    }
}

/// Restriction on hours as a trailing qualifier, `None` for every hour.
fn hours_qualifier(h: &Shape, hours: &[u32]) -> Option<String> {
    match h {
        Shape::All => None,
        Shape::Single(hr) => Some(format!(
            "between {} and {}",
            clock(*hr, 0, 0),
            clock(*hr, 59, 0)
        )),
        Shape::Range(a, b) => Some(format!(
            "between {} and {}",
            clock(*a, 0, 0),
            clock(*b, 0, 0)
        )),
        Shape::Step { start, step, end } if end + step > 23 => {
            let base = format!("during every {} hour", ordinal(*step));
            Some(if *start == 0 {
                base
            } else {
                format!("{base} starting at {}", clock(*start, 0, 0))
            })
        }
        _ => {
            let list: Vec<String> = hours.iter().map(u32::to_string).collect();
            Some(format!("during hours {}", join_and(&list)))
        }
    }
}
