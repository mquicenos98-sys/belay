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
