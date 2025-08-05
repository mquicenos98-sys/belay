//! The canonical text form of an expression, used by `Display`.
//!
//! The output is built from the field sets rather than the original text, so
//! equivalent spellings print identically: `0,15,30,45` becomes `*/15`,
//! `mon,tue,wed` becomes `MON-WED`, and nicknames print as their expansion.
//! Weekdays are always written as names, which reads better and means the
//! same thing in both dialects. The seconds field is printed only when it is
//! not a plain `0` (or the dialect requires it), the year field only when it
//! restricts anything.

use std::fmt;

use crate::expr::{bits, CronExpr, DomSpec, DowSpec};
use crate::parse::{Dialect, DAY_NAMES};

impl fmt::Display for CronExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let quartz = self.options.dialect() == Dialect::Quartz;
        let mut fields: Vec<String> = Vec::with_capacity(7);
        if quartz || self.has_seconds() || self.has_year() {
            fields.push(render_mask(self.seconds, 0, 59));
        }
        fields.push(render_mask(self.minutes, 0, 59));
        fields.push(render_mask(u64::from(self.hours), 0, 23));
        let mut dom = render_dom(&self.dom);
        let mut dow = render_dow(&self.dow);
        if quartz {
            // Quartz insists that exactly one day field is '?' unless both
            // are restricted.
            match (self.dom.is_restricted(), self.dow.is_restricted()) {
                (_, false) => dow = "?".to_string(),
                (false, true) => dom = "?".to_string(),
                (true, true) => {}
            }
        }
        fields.push(dom);
        fields.push(render_mask(u64::from(self.months), 1, 12));
        fields.push(dow);
        if self.has_year() {
            let years: Vec<u32> = self.years.iter().collect();
            fields.push(render_values(&years, crate::MIN_YEAR, crate::MAX_YEAR));
        }
        f.write_str(&fields.join(" "))
    }
}

fn render_mask(mask: u64, min: u32, max: u32) -> String {
    let values: Vec<u32> = bits(mask).collect();
    render_values(&values, min, max)
}

/// Renders sorted, distinct values of a field spanning `min..=max`.
pub(crate) fn render_values(values: &[u32], min: u32, max: u32) -> String {
    if values.len() as u32 == max - min + 1 {
        return "*".to_string();
    }
    if let Some(step) = progression(values, min, max) {
        let (first, last) = (values[0], values[values.len() - 1]);
        let reaches_end = last + step > max;
        return match (first == min, reaches_end) {
            (true, true) => format!("*/{step}"),
            (false, true) => format!("{first}-{max}/{step}"),
            _ => format!("{first}-{last}/{step}"),
        };
    }
    runs(values)
        .map(|(a, b)| match b - a {
            0 => a.to_string(),
            1 => format!("{a},{b}"),
            _ => format!("{a}-{b}"),
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The common difference if `values` form an arithmetic progression worth
/// writing as a step: at least three terms, or two terms that tile the field
/// from its start (`*/30` for minutes 0 and 30, but `0,45` stays a list).
pub(crate) fn progression(values: &[u32], min: u32, max: u32) -> Option<u32> {
    if values.len() < 2 {
        return None;
    }
    let step = values[1] - values[0];
    if step < 2 || values.windows(2).any(|w| w[1] - w[0] != step) {
        return None;
    }
    if values.len() >= 3 {
        return Some(step);
    }
    let tiles = values[0] == min && values[1] + step > max && (max - min + 1) % step == 0;
    tiles.then_some(step)
}

/// Maximal runs of consecutive values as inclusive `(start, end)` pairs.
pub(crate) fn runs(values: &[u32]) -> impl Iterator<Item = (u32, u32)> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        let start = *values.get(i)?;
        let mut end = start;
        i += 1;
        while values.get(i) == Some(&(end + 1)) {
            end += 1;
            i += 1;
        }
        Some((start, end))
    })
}

fn render_dom(dom: &DomSpec) -> String {
    if !dom.is_restricted() {
        return "*".to_string();
    }
    let mut items = Vec::new();
    if dom.days != 0 {
        items.push(render_mask(u64::from(dom.days), 1, 31));
    }
    if dom.last {
        items.push("L".to_string());
    }
    items.extend(bits(u64::from(dom.before_last)).map(|n| format!("L-{n}")));
    items.extend(bits(u64::from(dom.nearest)).map(|n| format!("{n}W")));
    if dom.last_weekday {
        items.push("LW".to_string());
    }
    items.join(",")
}

fn render_dow(dow: &DowSpec) -> String {
    if !dow.is_restricted() {
        return "*".to_string();
    }
    let mut items: Vec<String> = weekday_runs(dow.days)
        .into_iter()
        .map(|(a, len)| {
            let name = |i: u32| DAY_NAMES[((a + i) % 7) as usize];
            match len {
                1 => name(0).to_string(),
                2 => format!("{},{}", name(0), name(1)),
                _ => format!("{}-{}", name(0), name(len - 1)),
            }
        })
        .collect();
    items.extend(bits(u64::from(dow.last)).map(|d| format!("{}L", DAY_NAMES[d as usize])));
    for (d, nth) in dow.nth.iter().enumerate() {
        items.extend(bits(u64::from(*nth)).map(|n| format!("{}#{n}", DAY_NAMES[d])));
    }
    items.join(",")
}

/// Runs of consecutive weekdays as `(first, length)`. A run through
/// Saturday into Sunday is joined into one (`FRI-MON`) when that makes it long
/// enough to print as a range; otherwise the order is plain Sunday-first.
pub(crate) fn weekday_runs(days: u8) -> Vec<(u32, u32)> {
    let values: Vec<u32> = bits(u64::from(days)).collect();
    let mut out: Vec<(u32, u32)> = runs(&values).map(|(a, b)| (a, b - a + 1)).collect();
    if out.len() >= 2 {
        let (first, last) = (out[0], out[out.len() - 1]);
        if first.0 == 0 && last.0 + last.1 == 7 && first.1 + last.1 >= 3 {
            out.remove(0);
            let merged = out.len() - 1;
            out[merged].1 += first.1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CronExpr, ParseOptions};

    fn canon(s: &str) -> String {
        CronExpr::parse(s).unwrap().to_string()
    }

    #[test]
    fn prints_steps_ranges_and_lists() {
        assert_eq!(canon("0,15,30,45 * * * *"), "*/15 * * * *");
        assert_eq!(canon("5,20,35,50 * * * *"), "5-59/15 * * * *");
        assert_eq!(canon("10-50/20 * * * *"), "10-59/20 * * * *");
        assert_eq!(canon("10-30/10 * * * *"), "10-30/10 * * * *");
        assert_eq!(canon("0,30 * * * *"), "*/30 * * * *");
        assert_eq!(canon("0,45 * * * *"), "0,45 * * * *");
        assert_eq!(canon("1,2,3,7,8 * * * *"), "1-3,7,8 * * * *");
        assert_eq!(canon("0 0-23 * * *"), "0 * * * *");
        assert_eq!(canon("0 0 * 1,4,7,10 *"), "0 0 * */3 *");
    }

    #[test]
    fn prints_weekdays_as_names_with_circular_runs() {
        assert_eq!(canon("0 9 * * 1-5"), "0 9 * * MON-FRI");
        assert_eq!(canon("0 9 * * 5-7"), "0 9 * * FRI-SUN");
        assert_eq!(canon("0 9 * * 0,1,5,6"), "0 9 * * FRI-MON");
        assert_eq!(canon("0 9 * * 6,0"), "0 9 * * SUN,SAT");
        assert_eq!(canon("0 9 * * 0,1,3,5,6"), "0 9 * * WED,FRI-MON");
        assert_eq!(canon("0 9 * * 1,3,5"), "0 9 * * MON,WED,FRI");
        assert_eq!(canon("0 9 * * 0-6"), "0 9 * * *");
    }

    #[test]
    fn prints_special_day_items() {
        assert_eq!(canon("0 0 l * *"), "0 0 L * *");
        assert_eq!(canon("0 0 1,15,L-3,15w,lw * *"), "0 0 1,15,L-3,15W,LW * *");
        assert_eq!(canon("0 0 * * 5L,mon#2"), "0 0 * * FRIL,MON#2");
        assert_eq!(canon("0 0 * * MON,FRIL"), "0 0 * * MON,FRIL");
    }

    #[test]
    fn prints_seconds_and_years_only_when_needed() {
        assert_eq!(canon("0 30 9 * * *"), "30 9 * * *");
        assert_eq!(canon("*/10 30 9 * * *"), "*/10 30 9 * * *");
        assert_eq!(canon("0 30 9 * * * *"), "30 9 * * *");
        assert_eq!(canon("0 30 9 * * * 2025-2027"), "0 30 9 * * * 2025-2027");
        assert_eq!(canon("@hourly"), "0 * * * *");
        assert_eq!(canon("@yearly"), "0 0 1 1 *");
    }

    #[test]
    fn quartz_output_keeps_seconds_and_question_marks() {
        let q = |s: &str| {
            CronExpr::parse_with(s, ParseOptions::quartz())
                .unwrap()
                .to_string()
        };
        assert_eq!(q("0 15 10 ? * 2-6"), "0 15 10 ? * MON-FRI");
        assert_eq!(q("0 0 12 1 * ?"), "0 0 12 1 * ?");
        assert_eq!(q("0 0 12 * * ?"), "0 0 12 * * ?");
        assert_eq!(q("0 0 12 13 * 6"), "0 0 12 13 * FRI");
    }

    #[test]
    fn weekday_runs_wrap_across_saturday() {
        assert_eq!(weekday_runs(0b0110_0011), vec![(5, 4)]);
        assert_eq!(weekday_runs(0b0011_1110), vec![(1, 5)]);
        assert_eq!(
            weekday_runs(0b0101_0101),
            vec![(0, 1), (2, 1), (4, 1), (6, 1)]
        );
        assert_eq!(weekday_runs(0b0100_0001), vec![(0, 1), (6, 1)]);
    }

    #[test]
    fn progression_requires_regular_steps() {
        assert_eq!(progression(&[0, 20, 40], 0, 59), Some(20));
        assert_eq!(progression(&[0, 20, 45], 0, 59), None);
        assert_eq!(progression(&[1, 2, 3], 0, 59), None);
        assert_eq!(progression(&[0, 12], 0, 23), Some(12));
        assert_eq!(progression(&[5, 17], 0, 23), None);
    }
}
