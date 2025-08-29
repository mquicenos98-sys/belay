//! Tuning knobs for the two engines. The defaults suit a scheduler ticked
//! about once a second; every value is validated when an engine opens.

use std::time::Duration;

use belay_common::format_duration;
use belay_proto::limits::MAX_UNIQUE_FOR;

use crate::error::{Error, Result};

/// Shortest deduplication window: the unique scope `for:<window>` must be
/// at least a second.
const MIN_DEDUPE_WINDOW: Duration = Duration::from_secs(1);

/// Options for [`Scheduler`](crate::Scheduler).
///
/// ```
/// use std::time::Duration;
/// use belay_scheduler::SchedulerOptions;
///
/// let opts = SchedulerOptions::new()
///     .misfire_threshold(Duration::from_secs(30))
///     .dedupe_window(Duration::from_secs(7 * 86_400));
/// assert_eq!(opts.get_misfire_threshold(), Duration::from_secs(30));
/// assert!(opts.validate().is_ok());
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchedulerOptions {
    misfire_threshold: Duration,
    dedupe_window: Duration,
    retry_delay: Duration,
}

impl Default for SchedulerOptions {
    fn default() -> SchedulerOptions {
        SchedulerOptions {
            misfire_threshold: Duration::from_secs(60),
            dedupe_window: Duration::from_secs(24 * 3600),
            retry_delay: Duration::from_secs(5),
        }
    }
}

impl SchedulerOptions {
    /// The defaults: a 60 second misfire threshold, a 24 hour
    /// deduplication window and a 5 second retry delay.
    #[must_use]
    pub fn new() -> SchedulerOptions {
        SchedulerOptions::default()
    }

    /// How late a fire may be processed and still count as on time. A fire
    /// found later than this (because belay was down, or the tick loop
    /// stalled) is a *misfire*, handled by the schedule's
    /// [`MisfirePolicy`](belay_proto::MisfirePolicy).
    #[must_use]
    pub fn misfire_threshold(mut self, threshold: Duration) -> SchedulerOptions {
        self.misfire_threshold = threshold;
        self
    }

    /// How long the unique key of a fire (`sched:<id>:<fire ms>`) stays
    /// claimed after the job is created. A fire retried within this window,
    /// after a crash between dispatching and saving, finds the original job
    /// instead of creating a second one. Between 1s and 30d.
    #[must_use]
    pub fn dedupe_window(mut self, window: Duration) -> SchedulerOptions {
        self.dedupe_window = window;
        self
    }

    /// How long a schedule waits before retrying a fire the dispatcher
    /// could not take ([`DispatchError::Unavailable`](crate::DispatchError::Unavailable)).
    #[must_use]
    pub fn retry_delay(mut self, delay: Duration) -> SchedulerOptions {
        self.retry_delay = delay;
        self
    }

    /// See [`misfire_threshold`](Self::misfire_threshold).
    #[must_use]
    pub fn get_misfire_threshold(&self) -> Duration {
        self.misfire_threshold
    }

    /// See [`dedupe_window`](Self::dedupe_window).
    #[must_use]
    pub fn get_dedupe_window(&self) -> Duration {
        self.dedupe_window
    }

    /// See [`retry_delay`](Self::retry_delay).
    #[must_use]
    pub fn get_retry_delay(&self) -> Duration {
        self.retry_delay
    }

    /// Checks every value, naming the first one out of range.
    pub fn validate(&self) -> Result<()> {
        check_window(self.dedupe_window)?;
        if self.retry_delay.is_zero() {
            return Err(Error::InvalidOptions(
                "retry_delay must be positive".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Options for [`WorkflowEngine`](crate::WorkflowEngine).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowOptions {
    dedupe_window: Duration,
}

impl Default for WorkflowOptions {
    fn default() -> WorkflowOptions {
        WorkflowOptions {
            dedupe_window: Duration::from_secs(24 * 3600),
        }
    }
}

impl WorkflowOptions {
    /// The defaults: a 24 hour deduplication window.
    #[must_use]
    pub fn new() -> WorkflowOptions {
        WorkflowOptions::default()
    }

    /// How long the unique key of a step's job (`wf:<run>:<step>`) stays
    /// claimed, so that re-dispatching a step during recovery finds the job
    /// dispatched before a crash. Between 1s and 30d.
    #[must_use]
    pub fn dedupe_window(mut self, window: Duration) -> WorkflowOptions {
        self.dedupe_window = window;
        self
    }

    /// See [`dedupe_window`](Self::dedupe_window).
    #[must_use]
    pub fn get_dedupe_window(&self) -> Duration {
        self.dedupe_window
    }

    /// Checks every value.
    pub fn validate(&self) -> Result<()> {
        check_window(self.dedupe_window)
    }
}

fn check_window(window: Duration) -> Result<()> {
    if window < MIN_DEDUPE_WINDOW || window > MAX_UNIQUE_FOR {
        return Err(Error::InvalidOptions(format!(
            "dedupe_window must be between {} and {}, got {}",
            format_duration(MIN_DEDUPE_WINDOW),
            format_duration(MAX_UNIQUE_FOR),
            format_duration(window)
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        assert!(SchedulerOptions::new().validate().is_ok());
        assert!(WorkflowOptions::new().validate().is_ok());
        assert_eq!(
            SchedulerOptions::new().get_dedupe_window(),
            Duration::from_secs(86_400)
        );
    }

    #[test]
    fn out_of_range_values_are_named() {
        let err = SchedulerOptions::new()
            .dedupe_window(Duration::from_secs(40 * 86_400))
            .validate()
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid options: dedupe_window must be between 1s and 30d, got 40d"
        );
        let err = SchedulerOptions::new()
            .retry_delay(Duration::ZERO)
            .validate()
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid options: retry_delay must be positive"
        );
        assert!(WorkflowOptions::new()
            .dedupe_window(Duration::from_millis(10))
            .validate()
            .is_err());
    }
}
