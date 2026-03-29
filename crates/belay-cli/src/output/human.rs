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
