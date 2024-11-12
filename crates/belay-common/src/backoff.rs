//! Retry delay policies with jitter, and their compact text form.
//!
//! ```text
//! const:5s                 5s every time
//! linear:1s+2s..1m         1s, 3s, 5s, ... capped at 1m
//! exp:1s*2..10m            1s, 2s, 4s, ... capped at 10m
//! exp:500ms*1.5            uncapped (the `..max` part is optional)
//! exp:1s*2..10m~full       with jitter: ~full, ~equal, ~none or ~<p>%
//! ```
//!
//! Durations use the [`duration`](crate::duration) grammar. `constant:` and
//! `exponential:` are accepted as long spellings. `Display` produces the
//! canonical form, which parses back to an equal policy.
//!
//! All arithmetic saturates: a huge attempt number or factor yields the cap
//! (or `Duration::MAX` when uncapped), never a panic.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use crate::duration::{parse_duration, CompactDuration, ParseDurationError};
use crate::rand::Rng;

/// How the base delay grows with the attempt number.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Backoff {
    /// The same delay before every retry.
    Constant {
        /// The delay.
        delay: Duration,
    },
    /// `initial + step * (attempt - 1)`, capped at `max`.
    Linear {
        /// Delay before the first retry.
        initial: Duration,
        /// Added for each further retry.
        step: Duration,
        /// Upper bound (`Duration::MAX` for none).
        max: Duration,
    },
    /// `initial * factor^(attempt - 1)`, capped at `max`.
    Exponential {
        /// Delay before the first retry.
        initial: Duration,
        /// Growth per retry; values below 1 (or not finite) act as 1.
        factor: f64,
        /// Upper bound (`Duration::MAX` for none).
        max: Duration,
    },
}

impl Backoff {
    /// The delay before retry number `attempt` (1 = first retry), without
    /// jitter. Attempt 0 is treated as 1.
    #[must_use]
    pub fn base_delay(&self, attempt: u32) -> Duration {
        let n = attempt.saturating_sub(1);
        match *self {
            Backoff::Constant { delay } => delay,
            Backoff::Linear { initial, step, max } => step
                .checked_mul(n)
                .and_then(|grown| initial.checked_add(grown))
                .unwrap_or(Duration::MAX)
                .min(max),
            Backoff::Exponential {
                initial,
                factor,
                max,
            } => {
                let factor = if factor.is_finite() && factor >= 1.0 {
                    factor
                } else {
                    1.0
                };
                let exponent = i32::try_from(n).unwrap_or(i32::MAX);
                let nanos = initial.as_nanos() as f64 * factor.powi(exponent);
                if nanos >= max.as_nanos() as f64 {
                    max
                } else {
                    nanos_to_duration(nanos).min(max)
                }
            }
        }
    }

    /// The largest base delay this backoff can produce.
    #[must_use]
    pub fn max_delay(&self) -> Duration {
        match *self {
            Backoff::Constant { delay } => delay,
            Backoff::Linear { max, .. } | Backoff::Exponential { max, .. } => max,
        }
    }
}

/// Randomisation applied to the base delay so that many jobs failing
/// together do not retry in lockstep. See the AWS Architecture Blog post
/// "Exponential Backoff And Jitter" for the trade-offs.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Jitter {
    /// Use the base delay as is.
    #[default]
    None,
    /// Uniform in `[0, d]`: the best spread, but some retries are immediate.
    Full,
    /// Uniform in `[d/2, d]`: keeps at least half of the delay.
    Equal,
    /// Uniform in `[d(1-p), d(1+p)]` for a fraction `p` in `0..=1`; the
    /// only mode that can exceed the base delay (and the cap).
    Proportional(f64),
}

impl Jitter {
    /// Applies the jitter to `delay`.
    pub fn apply(&self, delay: Duration, rng: &mut Rng) -> Duration {
        match *self {
            Jitter::None => delay,
            Jitter::Full => uniform(Duration::ZERO, delay, rng),
            Jitter::Equal => {
                let half = delay / 2;
                uniform(half, delay, rng)
            }
            Jitter::Proportional(p) => {
                let p = if p.is_nan() { 0.0 } else { p.clamp(0.0, 1.0) };
                let spread = nanos_to_duration(delay.as_nanos() as f64 * p);
                uniform(
                    delay.saturating_sub(spread),
                    delay.saturating_add(spread),
                    rng,
                )
            }
        }
    }
}

/// Converts a non-negative nanosecond count, saturating and rounding to the
/// nearest nanosecond so results do not depend on float-to-Duration details.
fn nanos_to_duration(nanos: f64) -> Duration {
    if nanos.is_nan() || nanos <= 0.0 {
        Duration::ZERO
    } else if nanos < u64::MAX as f64 {
        Duration::from_nanos(nanos.round() as u64)
    } else {
        Duration::try_from_secs_f64(nanos / 1e9).unwrap_or(Duration::MAX)
    }
}

/// Uniform in `[low, high]`.
fn uniform(low: Duration, high: Duration, rng: &mut Rng) -> Duration {
    if high <= low {
        return low;
    }
    let span = high - low;
    let offset = match u64::try_from(span.as_nanos()) {
        Ok(n) => Duration::from_nanos(rng.gen_range(0..=n)),
        // Spans over 584 years only arise from uncapped policies; a float
        // fraction is precise enough there.
        Err(_) => nanos_to_duration(span.as_nanos() as f64 * rng.gen_f64()),
    };
    low.saturating_add(offset).min(high)
}

/// A backoff curve plus jitter: what a queue or client consults to decide
/// how long to wait before the next attempt.
///
/// ```
/// use belay_common::backoff::BackoffPolicy;
/// use belay_common::rand::Rng;
/// use std::time::Duration;
///
/// let policy: BackoffPolicy = "exp:1s*2..10m".parse().unwrap();
/// let mut rng = Rng::seed_from_u64(0);
/// assert_eq!(policy.delay_for(1, &mut rng), Duration::from_secs(1));
/// assert_eq!(policy.delay_for(4, &mut rng), Duration::from_secs(8));
/// assert_eq!(policy.delay_for(30, &mut rng), Duration::from_secs(600));
/// assert_eq!(policy.to_string(), "exp:1s*2..10m");
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BackoffPolicy {
    /// The base delay curve.
    pub backoff: Backoff,
    /// Randomisation applied on top.
    pub jitter: Jitter,
}

impl BackoffPolicy {
    /// A policy from its parts.
    #[must_use]
    pub const fn new(backoff: Backoff, jitter: Jitter) -> BackoffPolicy {
        BackoffPolicy { backoff, jitter }
    }

    /// `const:<delay>` without jitter.
    #[must_use]
    pub const fn constant(delay: Duration) -> BackoffPolicy {
        BackoffPolicy::new(Backoff::Constant { delay }, Jitter::None)
    }

    /// `linear:<initial>+<step>..<max>` without jitter.
    #[must_use]
    pub const fn linear(initial: Duration, step: Duration, max: Duration) -> BackoffPolicy {
        BackoffPolicy::new(Backoff::Linear { initial, step, max }, Jitter::None)
    }

    /// `exp:<initial>*<factor>..<max>` without jitter.
    #[must_use]
    pub const fn exponential(initial: Duration, factor: f64, max: Duration) -> BackoffPolicy {
        BackoffPolicy::new(
            Backoff::Exponential {
                initial,
                factor,
                max,
            },
            Jitter::None,
        )
    }

    /// The same policy with different jitter.
    #[must_use]
    pub const fn with_jitter(self, jitter: Jitter) -> BackoffPolicy {
        BackoffPolicy::new(self.backoff, jitter)
    }

    /// How long to wait before retry number `attempt` (1 = the first retry).
    pub fn delay_for(&self, attempt: u32, rng: &mut Rng) -> Duration {
        self.jitter.apply(self.backoff.base_delay(attempt), rng)
    }
}

/// `exp:1s*2..1h~10%`: doubling from one second, capped at an hour, with
/// ±10% jitter so simultaneous failures spread out.
impl Default for BackoffPolicy {
    fn default() -> Self {
        BackoffPolicy::exponential(Duration::from_secs(1), 2.0, Duration::from_secs(3600))
            .with_jitter(Jitter::Proportional(0.1))
    }
}

impl fmt::Display for Backoff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cap = |f: &mut fmt::Formatter<'_>, max: Duration| {
            if max == Duration::MAX {
                Ok(())
            } else {
                write!(f, "..{}", CompactDuration(max))
            }
        };
        match *self {
            Backoff::Constant { delay } => write!(f, "const:{}", CompactDuration(delay)),
            Backoff::Linear { initial, step, max } => {
                write!(
                    f,
                    "linear:{}+{}",
                    CompactDuration(initial),
                    CompactDuration(step)
                )?;
                cap(f, max)
            }
            Backoff::Exponential {
                initial,
                factor,
                max,
            } => {
                write!(f, "exp:{}*{factor}", CompactDuration(initial))?;
                cap(f, max)
            }
        }
    }
}

impl fmt::Display for Jitter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Jitter::None => f.write_str("none"),
            Jitter::Full => f.write_str("full"),
            Jitter::Equal => f.write_str("equal"),
            // Round away float noise such as 0.07 * 100 = 7.000000000000001.
            Jitter::Proportional(p) => write!(f, "{}%", (p * 100.0 * 1e6).round() / 1e6),
        }
    }
}

impl fmt::Display for BackoffPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.backoff)?;
        match self.jitter {
            Jitter::None => Ok(()),
            jitter => write!(f, "~{jitter}"),
        }
    }
}

impl FromStr for Jitter {
    type Err = ParseBackoffError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_jitter(s).ok_or_else(|| ParseBackoffError {
            input: s.to_owned(),
            kind: BackoffErrorKind::Jitter(s.to_owned()),
        })
    }
}

fn parse_jitter(s: &str) -> Option<Jitter> {
    match s {
        "none" => Some(Jitter::None),
        "full" => Some(Jitter::Full),
        "equal" => Some(Jitter::Equal),
        _ => s
            .strip_suffix('%')
            .and_then(|p| p.parse::<f64>().ok())
            .filter(|p| p.is_finite() && (0.0..=100.0).contains(p))
            .map(|p| Jitter::Proportional(p / 100.0)),
    }
}
