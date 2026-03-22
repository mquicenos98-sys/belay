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
            clock(*b, 59, 0)
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

/// The day phrase, and whether it already named the month.
fn describe_days(e: &CronExpr, months: &[u32]) -> Option<(String, bool)> {
    match (e.dom.is_restricted(), e.dow.is_restricted()) {
        (false, false) => None,
        (true, false) => {
            // "on January 1" reads better than "on day 1 of the month in January".
            if let ([month], Some(day)) = (months, single_day(&e.dom)) {
                return Some((format!("on {} {day}", MONTHS[*month as usize - 1]), true));
            }
            Some((format!("on {}", dom_phrase(&e.dom)), false))
        }
        (false, true) => Some((format!("on {}", dow_phrase(&e.dow)), false)),
        (true, true) => {
            let dom = dom_phrase(&e.dom);
            let dow = dow_phrase(&e.dow);
            Some(match e.options.day_match() {
                DayMatch::Either => (format!("on {dom} or on {dow}"), false),
                DayMatch::Both => (format!("on {dom} if it falls on {dow}"), false),
            })
        }
    }
}

fn single_day(dom: &DomSpec) -> Option<u32> {
    (!dom.has_specials() && dom.days.count_ones() == 1).then(|| dom.days.trailing_zeros())
}

fn dom_phrase(dom: &DomSpec) -> String {
    let days: Vec<u32> = bits(u64::from(dom.days)).collect();
    let mut items = Vec::new();
    if !days.is_empty() {
        items.push(match Shape::of(&days, 1, 31) {
            Shape::Single(d) => format!("day {d}"),
            Shape::Range(a, b) => format!("days {a} through {b}"),
            Shape::Step { start, step, end } if end + step > 31 => {
                if start == 1 {
                    format!("every {} day", ordinal(step))
                } else {
                    format!("every {} day from day {start}", ordinal(step))
                }
            }
            _ => {
                let list: Vec<String> = runs(&days)
                    .map(|(a, b)| match b - a {
                        0 => a.to_string(),
                        1 => format!("{a}, {b}"),
                        _ => format!("{a} through {b}"),
                    })
                    .collect();
                format!("days {}", join_and(&list))
            }
        });
    }
    if dom.last {
        items.push("the last day".to_string());
    }
    for n in bits(u64::from(dom.before_last)) {
        items.push(format!("{n} {} before the last day", plural(n, "day")));
    }
    for n in bits(u64::from(dom.nearest)) {
        items.push(format!("the weekday nearest day {n}"));
    }
    if dom.last_weekday {
        items.push("the last weekday".to_string());
    }
    format!("{} of the month", join_and(&items))
}

fn dow_phrase(dow: &DowSpec) -> String {
    let mut items: Vec<String> = Vec::new();
    for (start, len) in weekday_runs(dow.days) {
        let name = |i: u32| Weekday::from_index(start + i).name();
        match len {
            1 => items.push(name(0).to_string()),
            2 => {
                items.push(name(0).to_string());
                items.push(name(1).to_string());
            }
            _ => items.push(format!("{} through {}", name(0), name(len - 1))),
        }
    }
    let mut month_relative = Vec::new();
    for d in bits(u64::from(dow.last)) {
        month_relative.push(format!("the last {}", Weekday::from_index(d).name()));
    }
    for (d, nth) in dow.nth.iter().enumerate() {
        if *nth != 0 {
            let which: Vec<String> = bits(u64::from(*nth))
                .map(|n| ordinal_word(n).to_string())
                .collect();
            month_relative.push(format!(
                "the {} {}",
                join_and(&which),
                Weekday::from_index(d as u32).name()
            ));
        }
    }
    if !month_relative.is_empty() {
        items.push(format!("{} of the month", join_and(&month_relative)));
    }
    join_and(&items)
}

fn month_list(months: &[u32]) -> String {
    let items: Vec<String> = runs(months)
        .flat_map(|(a, b)| {
            let name = |m: u32| MONTHS[m as usize - 1];
            match b - a {
                0 => vec![name(a).to_string()],
                1 => vec![name(a).to_string(), name(b).to_string()],
                _ => vec![format!("{} through {}", name(a), name(b))],
            }
        })
        .collect();
    join_and(&items)
}

fn describe_years(years: &[u32]) -> String {
    match Shape::of(years, crate::MIN_YEAR, crate::MAX_YEAR) {
        Shape::Single(y) => format!("in {y}"),
        Shape::Range(a, b) => format!("in {a} through {b}"),
        Shape::Step { start, step, end } if end + step > crate::MAX_YEAR => {
            format!("every {} year from {start}", ordinal(step))
        }
        _ => {
            let list: Vec<String> = runs(years)
                .map(|(a, b)| match b - a {
                    0 => a.to_string(),
                    1 => format!("{a}, {b}"),
                    _ => format!("{a} through {b}"),
                })
                .collect();
            format!("in {}", join_and(&list))
        }
    }
}

/// `HH:MM`, or `HH:MM:SS` when the second is not zero.
fn clock(h: u32, m: u32, s: u32) -> String {
    if s == 0 {
        format!("{h:02}:{m:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    }
}

/// "every 5 minutes" when the step divides the cycle evenly, otherwise
/// "every 7th minute" (the count restarts at the top of the cycle).
fn every(step: u32, unit: &str, cycle: u32) -> String {
    if cycle % step == 0 {
        format!("every {step} {unit}s")
    } else {
        format!("every {} {unit}", ordinal(step))
    }
}

fn ordinal(n: u32) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

fn ordinal_word(n: u32) -> &'static str {
    match n {
        1 => "first",
        2 => "second",
        3 => "third",
        4 => "fourth",
        _ => "fifth",
    }
}

fn plural(n: u32, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

/// "a", "a and b", "a, b and c".
fn join_and(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ParseOptions;

    fn d(s: &str) -> String {
        CronExpr::parse(s).unwrap().describe()
    }

    #[test]
    fn helpers_format_ordinals_and_lists() {
        assert_eq!(ordinal(1), "1st");
        assert_eq!(ordinal(2), "2nd");
        assert_eq!(ordinal(3), "3rd");
        assert_eq!(ordinal(11), "11th");
        assert_eq!(ordinal(22), "22nd");
        assert_eq!(ordinal(113), "113th");
        assert_eq!(join_and(&["a".into()]), "a");
        assert_eq!(join_and(&["a".into(), "b".into()]), "a and b");
        assert_eq!(
            join_and(&["a".into(), "b".into(), "c".into()]),
            "a, b and c"
        );
    }

    #[test]
    fn periodic_times() {
        assert_eq!(d("* * * * *"), "every minute");
        assert_eq!(d("*/5 * * * *"), "every 5 minutes");
        assert_eq!(d("0,30 * * * *"), "every 30 minutes");
        assert_eq!(d("*/7 * * * *"), "every 7th minute");
        assert_eq!(d("5/15 * * * *"), "every 15 minutes starting at minute 5");
        assert_eq!(d("0 * * * *"), "every hour");
        assert_eq!(d("30 * * * *"), "at 30 minutes past every hour");
        assert_eq!(d("1 * * * *"), "at 1 minute past every hour");
        assert_eq!(d("0 */2 * * *"), "every 2 hours");
        assert_eq!(
            d("0 */5 * * *"),
            "at 00:00, 05:00, 10:00, 15:00 and 20:00 every day"
        );
        assert_eq!(d("0 */6 * * *"), "every 6 hours");
        assert_eq!(d("0 */12 * * *"), "every 12 hours");
        assert_eq!(d("0 1/2 * * *"), "every 2 hours starting at 01:00");
        assert_eq!(d("15 */3 * * *"), "at 15 minutes past every 3rd hour");
        assert_eq!(d("0 1/6 * * *"), "every 6 hours starting at 01:00");
    }

    #[test]
    fn exact_times() {
        assert_eq!(d("30 9 * * *"), "at 09:30 every day");
        assert_eq!(d("0 9,17 * * *"), "at 09:00 and 17:00 every day");
        assert_eq!(d("15 30 9 * * *"), "at 09:30:15 every day");
        assert_eq!(d("@daily"), "at 00:00 every day");
    }

    #[test]
    fn times_within_hours() {
        assert_eq!(
            d("*/15 9-17 * * *"),
            "every 15 minutes between 09:00 and 17:59"
        );
        assert_eq!(d("* 9 * * *"), "every minute between 09:00 and 09:59");
        assert_eq!(d("0 9-17 * * *"), "every hour between 09:00 and 17:59");
        assert_eq!(
            d("0,15,45 * * * *"),
            "at minutes 0, 15 and 45 past every hour"
        );
        assert_eq!(
            d("10-20 * * * *"),
            "at minutes 10 through 20 past every hour"
        );
        assert_eq!(
            d("*/10 * 1,3,5,7,9,20,22 * * *"),
            "every 10 seconds during hours 1, 3, 5, 7, 9, 20 and 22"
        );
    }

    #[test]
    fn seconds() {
        assert_eq!(d("* * * * * *"), "every second");
        assert_eq!(d("*/10 * * * * *"), "every 10 seconds");
        assert_eq!(d("30 * * * * *"), "every minute, at second 30");
        assert_eq!(
            d("*/5 30 9 * * *"),
            "every 5 seconds between 09:30 and 09:30:59"
        );
    }

    #[test]
    fn days_of_month() {
        assert_eq!(d("0 0 L * *"), "at 00:00 on the last day of the month");
        assert_eq!(d("0 0 1 * *"), "at 00:00 on day 1 of the month");
        assert_eq!(d("0 0 1,15 * *"), "at 00:00 on days 1 and 15 of the month");
        assert_eq!(
            d("0 0 1,2,15 * *"),
            "at 00:00 on days 1, 2 and 15 of the month"
        );
        assert_eq!(
            d("0 0 1-7 * *"),
            "at 00:00 on days 1 through 7 of the month"
        );
        assert_eq!(
            d("0 0 L-3 * *"),
            "at 00:00 on 3 days before the last day of the month"
        );
        assert_eq!(
            d("0 0 15W * *"),
            "at 00:00 on the weekday nearest day 15 of the month"
        );
        assert_eq!(d("0 0 LW * *"), "at 00:00 on the last weekday of the month");
        assert_eq!(
            d("0 0 1,L * *"),
            "at 00:00 on day 1 and the last day of the month"
        );
        assert_eq!(d("0 0 */2 * *"), "at 00:00 on every 2nd day of the month");
    }

    #[test]
    fn days_of_week() {
        assert_eq!(d("30 9 * * 1-5"), "at 09:30 on Monday through Friday");
        assert_eq!(d("0 0 * * 0"), "at 00:00 on Sunday");
        assert_eq!(
            d("0 0 * * 1,3,5"),
            "at 00:00 on Monday, Wednesday and Friday"
        );
        assert_eq!(d("0 0 * * FRI-MON"), "at 00:00 on Friday through Monday");
        assert_eq!(
            d("0 0 * * FRIL"),
            "at 00:00 on the last Friday of the month"
        );
        assert_eq!(
            d("0 0 * * MON#1,MON#3"),
            "at 00:00 on the first and third Monday of the month"
        );
    }

    #[test]
    fn day_fields_combined() {
        assert_eq!(
            d("0 0 13 * FRI"),
            "at 00:00 on day 13 of the month or on Friday"
        );
        let both = CronExpr::parse_with(
            "0 0 13 * FRI",
            ParseOptions::new().with_day_match(DayMatch::Both),
        )
        .unwrap();
        assert_eq!(
            both.describe(),
            "at 00:00 on day 13 of the month if it falls on Friday"
        );
    }

    #[test]
    fn months_and_years() {
        assert_eq!(d("0 0 1 1 *"), "at 00:00 on January 1");
        assert_eq!(
            d("0 12 * 6-8 *"),
            "at 12:00 every day in June through August"
        );
        assert_eq!(
            d("0 0 1 */3 *"),
            "at 00:00 on day 1 of the month in January, April, July and October"
        );
        assert_eq!(d("0 0 0 1 1 * 2030"), "at 00:00 on January 1 in 2030");
        assert_eq!(
            d("0 0 0 * * MON 2025-2027"),
            "at 00:00 on Monday in 2025 through 2027"
        );
        assert_eq!(
            d("0 0 0 1 1 * 2030/5"),
            "at 00:00 on January 1 every 5th year from 2030"
        );
    }
}
