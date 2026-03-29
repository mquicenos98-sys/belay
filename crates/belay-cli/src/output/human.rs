//! Rounded, human-sized renderings of times and durations for tables and
//! detail views. Exact values are always available with `--json`.

use std::time::Duration;

use belay_common::Timestamp;

/// A duration rounded to at most two units: `450ms`, `4.2s`, `12s`,
/// `3m20s`, `2h5m`, `3d4h`.
#[must_use]
pub(crate) fn approx(d: Duration) -> String {
    let ms = d.as_millis();
    let secs = d.as_secs();
    if ms < 1_000 {
        return format!("{ms}ms");
    }
    if secs < 10 {
        return format!("{:.1}s", d.as_secs_f64());
    }
    if secs < 60 {
        return format!("{secs}s");
    }
    let two = |big: u64, big_unit: &str, small: u64, small_unit: &str| {
        if small == 0 {
            format!("{big}{big_unit}")
        } else {
            format!("{big}{big_unit}{small}{small_unit}")
        }
    };
    if secs < 3_600 {
        return two(secs / 60, "m", secs % 60, "s");
    }
    if secs < 86_400 {
        return two(secs / 3_600, "h", secs % 3_600 / 60, "m");
    }
    two(secs / 86_400, "d", secs % 86_400 / 3_600, "h")
}

/// `then` relative to `now`: `just now`, `5s ago`, `in 3m`.
#[must_use]
pub(crate) fn relative(then: Timestamp, now: Timestamp) -> String {
    match now.duration_since(then) {
        Some(past) if past < Duration::from_secs(1) => "just now".to_owned(),
        Some(past) => format!("{} ago", coarse(past)),
        None => {
            let future = then.saturating_duration_since(now);
            if future < Duration::from_secs(1) {
                "just now".to_owned()
            } else {
                format!("in {}", coarse(future))
            }
        }
    }
}

/// One unit only, for relative times: `5s`, `3m`, `2h`, `4d`.
fn coarse(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3_599 => format!("{}m", secs / 60),
        3_600..=86_399 => format!("{}h", secs / 3_600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// An absolute time with its relative reading, for detail views:
/// `2025-03-04T05:06:07.089Z (3m ago)`.
#[must_use]
pub(crate) fn when(then: Timestamp, now: Timestamp) -> String {
    format!("{} ({})", then.to_rfc3339(), relative(then, now))
}

/// A progress fraction as a percentage with at most one decimal.
#[must_use]
pub(crate) fn percent(value: f32) -> String {
    if (value - value.round()).abs() < 0.05 {
        format!("{:.0}%", value)
    } else {
        format!("{value:.1}%")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(secs: i64) -> Timestamp {
        Timestamp::from_secs(1_700_000_000 + secs)
    }

    #[test]
    fn durations_round_to_two_units() {
        let cases = [
            (Duration::ZERO, "0ms"),
            (Duration::from_millis(450), "450ms"),
            (Duration::from_millis(4_260), "4.3s"),
            (Duration::from_secs(12), "12s"),
            (Duration::from_secs(200), "3m20s"),
            (Duration::from_secs(180), "3m"),
            (Duration::from_secs(7_500), "2h5m"),
            (Duration::from_secs(3 * 86_400 + 4 * 3_600 + 59), "3d4h"),
        ];
        for (d, text) in cases {
            assert_eq!(approx(d), text, "{d:?}");
        }
    }

    #[test]
    fn relative_times_read_naturally_both_ways() {
        let now = ts(0);
        assert_eq!(relative(now, now), "just now");
        assert_eq!(relative(ts(-5), now), "5s ago");
        assert_eq!(relative(ts(-190), now), "3m ago");
        assert_eq!(relative(ts(-7_300), now), "2h ago");
        assert_eq!(relative(ts(-90_000), now), "1d ago");
        assert_eq!(relative(ts(125), now), "in 2m");
        assert_eq!(
            when(ts(-60), now),
            format!("{} (1m ago)", ts(-60).to_rfc3339())
        );
    }

    #[test]
    fn percentages_drop_needless_decimals() {
        assert_eq!(percent(50.0), "50%");
        assert_eq!(percent(12.34), "12.3%");
        assert_eq!(percent(99.98), "100%");
    }
}
