//! Property tests: the field-wise iterator against slow, obviously-correct
//! references, over random expressions from a seeded generator.
//!
//! The time references step through every minute (or second) of a window,
//! read the wall clock with `Tz::to_local`, and apply the documented DST
//! rules directly: a reading in a forward gap fires at the transition, a
//! repeated reading fires on its second occurrence only for periodic
//! expressions. The date reference re-implements the day-field semantics
//! (`L`, `W`, `#`, OR/AND) from scratch with its own calendar arithmetic.

use belay_cron::{CronExpr, DayMatch, LocalDateTime, LocalResult, ParseOptions, Tz};

/// SplitMix64: tiny, seedable, good enough to drive test generation.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u32) -> u32 {
        (self.next() % u64::from(n)) as u32
    }

    fn range(&mut self, lo: u32, hi: u32) -> u32 {
        lo + self.below(hi - lo + 1)
    }

    fn chance(&mut self, percent: u32) -> bool {
        self.below(100) < percent
    }
}

/// A random field over `lo..=hi`: wildcards, values, ranges, steps, lists
/// and (when `wrap` is set) ranges that wrap around.
fn numeric_field(rng: &mut Rng, lo: u32, hi: u32, wrap: bool) -> String {
    let span = hi - lo + 1;
    match rng.below(10) {
        0 | 1 => "*".to_string(),
        2 => format!("*/{}", rng.range(2, span / 2)),
        3 => rng.range(lo, hi).to_string(),
        4 => {
            let a = rng.range(lo, hi - 1);
            format!("{a}-{}", rng.range(a + 1, hi))
        }
        5 => {
            let a = rng.range(lo, hi - 2);
            let b = rng.range(a + 2, hi);
            format!("{a}-{b}/{}", rng.range(2, (b - a).max(2)))
        }
        6 => {
            let n = rng.range(2, 4);
            let v: Vec<String> = (0..n).map(|_| rng.range(lo, hi).to_string()).collect();
            v.join(",")
        }
        7 => format!("{}/{}", rng.range(lo, hi), rng.range(2, span / 2)),
        8 if wrap => {
            let a = rng.range(lo + 1, hi);
            format!("{a}-{}", rng.range(lo, a - 1))
        }
        _ => format!("{},{}", rng.range(lo, hi), rng.range(lo, hi)),
    }
}

/// Hours biased towards the small hours, where DST transitions happen.
fn hour_field(rng: &mut Rng) -> String {
    if rng.chance(40) {
        match rng.below(5) {
            0 => format!("{}", rng.range(0, 3)),
            1 => format!("{}-{}", rng.range(0, 1), rng.range(2, 4)),
            2 => "1,2,3".to_string(),
            3 => "*/2".to_string(),
            _ => format!("0-{}/{}", rng.range(3, 6), rng.range(1, 2)),
        }
    } else {
        numeric_field(rng, 0, 23, true)
    }
}

fn minute_field(rng: &mut Rng) -> String {
    if rng.chance(30) {
        ["0", "30", "*/15", "*/30", "0,30", "45", "*/7", "59"][rng.below(8) as usize].to_string()
    } else {
        numeric_field(rng, 0, 59, true)
    }
}

/// Day fields are mostly unrestricted so that short windows see fires.
fn day_fields(rng: &mut Rng) -> (String, String, String) {
    let dom = if rng.chance(80) {
        "*".to_string()
    } else {
        ["1-15", "L", "15W", "*/2", "LW", "10-31", "L-2,1"][rng.below(7) as usize].to_string()
    };
    let month = if rng.chance(85) {
        "*".to_string()
    } else {
        ["3,10,11", "*/2", "4-9", "OCT-APR"][rng.below(4) as usize].to_string()
    };
    let dow = if rng.chance(80) {
        "*".to_string()
    } else {
        ["1-5", "0,6", "SUN", "FRI-MON", "SUNL", "SUN#1,SUN#2"][rng.below(6) as usize].to_string()
    };
    (dom, month, dow)
}

fn random_expr(rng: &mut Rng) -> String {
    if rng.chance(20) {
        // A fixed daily time in the small hours: the case where the "fire
        // once" rules for skipped and repeated readings matter most.
        let minute = rng.range(0, 59);
        let hours = ["1", "2", "0", "3", "1,2", "0-3"][rng.below(6) as usize];
        return format!("{minute} {hours} * * *");
    }
    let (dom, month, dow) = day_fields(rng);
    format!(
        "{} {} {dom} {month} {dow}",
        minute_field(rng),
        hour_field(rng)
    )
}

fn random_seconds_expr(rng: &mut Rng) -> String {
    let seconds = if rng.chance(50) {
        ["*/10", "0,30", "*/20", "5-10", "0"][rng.below(5) as usize].to_string()
    } else {
        numeric_field(rng, 0, 59, true)
    };
    let minutes = ["*", "*/15", "0", "59", "0-5", "*/2"][rng.below(6) as usize];
    let hours = ["*", "1", "1-3", "0,2", "2"][rng.below(5) as usize];
    format!("{seconds} {minutes} {hours} * * *")
}

/// Every fire instant in `(from, to]` by exhaustive stepping.
fn reference_fires(expr: &CronExpr, tz: &Tz, from: i64, to: i64, step: i64) -> Vec<i64> {
    let mut out = Vec::new();
    let mut t = (from.div_euclid(step) + 1) * step;
    while t <= to {
        let local = tz.to_local(t);
        if expr.matches_local(&local) {
            let second_pass =
                matches!(tz.from_local(local), LocalResult::Ambiguous(first, _) if first != t);
            if !second_pass || expr.is_periodic() {
                out.push(t);
            }
        }
        t += step;
    }
    let mut cursor = from;
    while let Some(tr) = tz.next_transition(cursor) {
        if tr.at > to {
            break;
        }
        if tr.is_forward() {
            let mut reading = tr.at + i64::from(tr.offset_before);
            let gap_end = tr.at + i64::from(tr.offset_after);
            while reading < gap_end {
                if expr.matches_local(&Tz::Utc.to_local(reading)) {
                    out.push(tr.at);
                    break;
                }
                reading += step;
            }
        }
        cursor = tr.at;
    }
    out.sort_unstable();
    out.dedup();
    out
}

fn fast_fires(expr: &CronExpr, tz: &Tz, from: i64, to: i64) -> Vec<i64> {
    expr.upcoming(from, tz).take_while(|&t| t <= to).collect()
}

fn fast_fires_backwards(expr: &CronExpr, tz: &Tz, from: i64, to: i64) -> Vec<i64> {
    let mut v: Vec<i64> = expr
        .preceding(to + 1, tz)
        .take_while(|&t| t > from)
        .collect();
    v.reverse();
    v
}

/// A window start near one of the zone's transitions in a random year, or
/// anywhere in 2020-2035 for zones without transitions.
fn window_start(rng: &mut Rng, tz: &Tz) -> i64 {
    let year_start = |y: i64| (y - 1970) * 365 * 86_400 + (y - 1969) / 4 * 86_400;
    let year = i64::from(rng.range(2020, 2035));
    let base = year_start(year);
    match tz.next_transition(base + i64::from(rng.below(365)) * 86_400) {
        // Exactly at the edges of a transition, where off-by-one errors live.
        Some(tr) if rng.chance(15) => tr.at + [-61, -60, -1, 0, 1][rng.below(5) as usize],
        Some(tr) if rng.chance(85) => tr.at - i64::from(rng.below(48 * 3600)),
        _ => base + i64::from(rng.below(365 * 86_400)),
    }
}

/// What a batch of cases exercised, so tests can insist on DST coverage.
#[derive(Debug, Default)]
struct Coverage {
    fires: usize,
    /// Fires at a transition instant whose own reading does not match.
    gap_fires: usize,
    /// Fires on the second occurrence of a repeated reading.
    second_pass_fires: usize,
    /// Repeated readings that matched but were skipped (non-periodic).
    skipped_repeats: usize,
}

fn check_zone(tz: &Tz, seed: u64, cases: usize) -> Coverage {
    let mut rng = Rng(seed);
    let mut cov = Coverage::default();
    for case in 0..cases {
        let text = random_expr(&mut rng);
        let expr = CronExpr::parse(&text).unwrap_or_else(|e| panic!("{text}: {e}"));
        let from = window_start(&mut rng, tz);
        let to = from + 3 * 86_400;
        let expected = reference_fires(&expr, tz, from, to, 60);
        let forward = fast_fires(&expr, tz, from, to);
        assert_eq!(
            forward, expected,
            "case {case}: `{text}` in {tz} from {from} (forward)"
        );
        let backward = fast_fires_backwards(&expr, tz, from, to);
        assert_eq!(
            backward, expected,
            "case {case}: `{text}` in {tz} from {from} (backward)"
        );
        cov.fires += expected.len();
        for &t in &expected {
            let local = tz.to_local(t);
            if !expr.matches_local(&local) {
                cov.gap_fires += 1;
            } else if matches!(tz.from_local(local), LocalResult::Ambiguous(a, _) if a != t) {
                cov.second_pass_fires += 1;
            }
        }
        if !expr.is_periodic() {
            let mut t = from - from.rem_euclid(60) + 60;
            while t <= to {
                let local = tz.to_local(t);
                if expr.matches_local(&local)
                    && matches!(tz.from_local(local), LocalResult::Ambiguous(a, _) if a != t)
                {
                    cov.skipped_repeats += 1;
                }
                t += 60;
            }
        }
    }
    // Guard against a generator that only produces empty windows.
    assert!(cov.fires > cases * 5, "only {} fires in {tz}", cov.fires);
    cov
}

/// Zones with DST must have exercised every rule at least once.
fn assert_dst_coverage(cov: &Coverage, tz: &str) {
    assert!(cov.gap_fires > 0, "{tz}: no gap fires in {cov:?}");
    assert!(
        cov.second_pass_fires > 0,
        "{tz}: no second-pass fires in {cov:?}"
    );
    assert!(
        cov.skipped_repeats > 0,
        "{tz}: no skipped repeats in {cov:?}"
    );
}

#[test]
fn utc_iteration_matches_minute_by_minute_reference() {
    check_zone(&Tz::Utc, 0x5EED_0001, 150);
}

#[test]
fn new_york_iteration_matches_reference_across_transitions() {
    let cov = check_zone(&Tz::named("America/New_York").unwrap(), 0x5EED_0002, 70);
    assert_dst_coverage(&cov, "America/New_York");
}

#[test]
fn london_iteration_matches_reference_across_transitions() {
    let cov = check_zone(&Tz::named("Europe/London").unwrap(), 0x5EED_0003, 70);
    assert_dst_coverage(&cov, "Europe/London");
}

#[test]
fn sydney_iteration_matches_reference_across_southern_transitions() {
    let cov = check_zone(&Tz::named("Australia/Sydney").unwrap(), 0x5EED_0004, 70);
    assert_dst_coverage(&cov, "Australia/Sydney");
}

#[test]
fn kolkata_half_hour_offset_matches_reference() {
    let cov = check_zone(&Tz::named("Asia/Kolkata").unwrap(), 0x5EED_0005, 40);
    assert_eq!(
        cov.gap_fires + cov.second_pass_fires + cov.skipped_repeats,
        0
    );
}

#[test]
fn bogota_without_dst_matches_reference() {
    let cov = check_zone(&Tz::named("America/Bogota").unwrap(), 0x5EED_0006, 40);
    assert_eq!(
        cov.gap_fires + cov.second_pass_fires + cov.skipped_repeats,
        0
    );
}

#[test]
fn unusual_rules_match_reference() {
    // Thirty-minute DST, negative DST, midnight transitions, 26:00 rules.
    for (i, name) in [
        "Australia/Lord_Howe",
        "Europe/Dublin",
        "America/Santiago",
        "Asia/Jerusalem",
        "America/Havana",
        "Pacific/Chatham",
    ]
    .iter()
    .enumerate()
    {
        let cov = check_zone(&Tz::named(name).unwrap(), 0x5EED_0100 + i as u64, 40);
        assert!(cov.gap_fires + cov.second_pass_fires > 0, "{name}: {cov:?}");
    }
}

#[test]
fn seconds_fields_match_second_by_second_reference() {
    let mut rng = Rng(0x5EED_0007);
    for tz in [Tz::Utc, Tz::named("America/New_York").unwrap()] {
        for case in 0..40 {
            let text = random_seconds_expr(&mut rng);
            let expr = CronExpr::parse(&text).unwrap();
            let from = match tz.next_transition(1_600_000_000 + i64::from(rng.below(1 << 28))) {
                Some(tr) => tr.at - i64::from(rng.range(1800, 5400)),
                None => 1_700_000_000 + i64::from(rng.below(1 << 26)),
            };
            let to = from + 2 * 3600;
            let expected = reference_fires(&expr, &tz, from, to, 1);
            assert_eq!(
                fast_fires(&expr, &tz, from, to),
                expected,
                "case {case}: `{text}` in {tz} from {from}"
            );
            assert_eq!(
                fast_fires_backwards(&expr, &tz, from, to),
                expected,
                "case {case}: `{text}` in {tz} from {from} (backward)"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Day fields against an independent model.

#[derive(Debug, Clone, Copy)]
enum DomItem {
    Day(u32),
    Range(u32, u32),
    Last,
    LastMinus(u32),
    Nearest(u32),
    LastWeekday,
}

#[derive(Debug, Clone, Copy)]
enum DowItem {
    Day(u32),
    Range(u32, u32),
    LastOf(u32),
    Nth(u32, u32),
}

#[derive(Debug)]
struct DateSpec {
    dom: Vec<DomItem>,
    months: Vec<u32>,
    dow: Vec<DowItem>,
    both: bool,
}

impl DateSpec {
    fn random(rng: &mut Rng) -> DateSpec {
        let mut dom = Vec::new();
        if rng.chance(70) {
            for _ in 0..rng.range(1, 3) {
                dom.push(match rng.below(6) {
                    0 => DomItem::Day(rng.range(1, 31)),
                    1 => {
                        let a = rng.range(1, 30);
                        DomItem::Range(a, rng.range(a, 31))
                    }
                    2 => DomItem::Last,
                    3 => DomItem::LastMinus(rng.range(1, 30)),
                    4 => DomItem::Nearest(rng.range(1, 31)),
                    _ => DomItem::LastWeekday,
                });
            }
        }
        let mut dow = Vec::new();
        if rng.chance(60) {
            for _ in 0..rng.range(1, 3) {
                dow.push(match rng.below(4) {
                    0 => DowItem::Day(rng.range(0, 6)),
                    1 => {
                        let a = rng.range(0, 5);
                        DowItem::Range(a, rng.range(a, 6))
                    }
                    2 => DowItem::LastOf(rng.range(0, 6)),
                    _ => DowItem::Nth(rng.range(0, 6), rng.range(1, 5)),
                });
            }
        }
        let months = if rng.chance(60) {
            Vec::new()
        } else {
            let mut m: Vec<u32> = (0..rng.range(1, 4)).map(|_| rng.range(1, 12)).collect();
            m.sort_unstable();
            m.dedup();
            m
        };
        DateSpec {
            dom,
            months,
            dow,
            both: rng.chance(40),
        }
    }

    fn render(&self) -> String {
        let dom = if self.dom.is_empty() {
            "*".to_string()
        } else {
            let items: Vec<String> = self
                .dom
                .iter()
                .map(|it| match *it {
                    DomItem::Day(d) => d.to_string(),
                    DomItem::Range(a, b) => format!("{a}-{b}"),
                    DomItem::Last => "L".to_string(),
                    DomItem::LastMinus(k) => format!("L-{k}"),
                    DomItem::Nearest(n) => format!("{n}W"),
                    DomItem::LastWeekday => "LW".to_string(),
                })
                .collect();
            items.join(",")
        };
        let months = if self.months.is_empty() {
            "*".to_string()
        } else {
            let items: Vec<String> = self.months.iter().map(u32::to_string).collect();
            items.join(",")
        };
        let dow = if self.dow.is_empty() {
            "?".to_string()
        } else {
            let items: Vec<String> = self
                .dow
                .iter()
                .map(|it| match *it {
                    DowItem::Day(d) => d.to_string(),
                    DowItem::Range(a, b) => format!("{a}-{b}"),
                    DowItem::LastOf(d) => format!("{d}L"),
                    DowItem::Nth(d, n) => format!("{d}#{n}"),
                })
                .collect();
            items.join(",")
        };
        format!("0 12 {dom} {months} {dow}")
    }

    fn matches(&self, y: i64, m: u32, d: u32) -> bool {
        if !self.months.is_empty() && !self.months.contains(&m) {
            return false;
        }
        let dim = ref_days_in_month(y, m);
        let wd = |day: u32| ref_weekday(y, m, day);
        let dom_ok = self.dom.iter().any(|it| match *it {
            DomItem::Day(x) => d == x,
            DomItem::Range(a, b) => (a..=b).contains(&d),
            DomItem::Last => d == dim,
            DomItem::LastMinus(k) => d + k == dim,
            DomItem::Nearest(n) => {
                n <= dim
                    && d == match wd(n) {
                        6 if n == 1 => 3,
                        6 => n - 1,
                        0 if n == dim => n - 2,
                        0 => n + 1,
                        _ => n,
                    }
            }
            DomItem::LastWeekday => {
                let mut last = dim;
                while wd(last) == 0 || wd(last) == 6 {
                    last -= 1;
                }
                d == last
            }
        });
        let w = wd(d);
        let dow_ok = self.dow.iter().any(|it| match *it {
            DowItem::Day(x) => w == x,
            DowItem::Range(a, b) => (a..=b).contains(&w),
            DowItem::LastOf(x) => w == x && d + 7 > dim,
            DowItem::Nth(x, n) => w == x && (d - 1) / 7 + 1 == n,
        });
        // A field is unrestricted when it lists nothing or covers every day.
        let dom_restricted = !self.dom.is_empty()
            && !(1..=31).all(|day| {
                self.dom.iter().any(|it| match *it {
                    DomItem::Day(x) => day == x,
                    DomItem::Range(a, b) => (a..=b).contains(&day),
                    _ => false,
                })
            });
        let dow_restricted = !self.dow.is_empty()
            && !(0..7).all(|x| {
                self.dow.iter().any(|it| match *it {
                    DowItem::Day(v) => v == x,
                    DowItem::Range(a, b) => (a..=b).contains(&x),
                    _ => false,
                })
            });
        match (dom_restricted, dow_restricted) {
            (false, false) => true,
            (true, false) => dom_ok,
            (false, true) => dow_ok,
            (true, true) if self.both => dom_ok && dow_ok,
            (true, true) => dom_ok || dow_ok,
        }
    }
}

fn ref_is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn ref_days_in_month(y: i64, m: u32) -> u32 {
    const DAYS: [u32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    DAYS[m as usize - 1] + u32::from(m == 2 && ref_is_leap(y))
}

/// Sakamoto's algorithm, 0 = Sunday.
fn ref_weekday(y: i64, m: u32, d: u32) -> u32 {
    const T: [i64; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let y = if m < 3 { y - 1 } else { y };
    ((y + y / 4 - y / 100 + y / 400 + T[m as usize - 1] + i64::from(d)) % 7) as u32
}
