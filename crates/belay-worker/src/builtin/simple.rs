//! [`EchoHandler`], [`SleepHandler`] and [`FailHandler`].

use std::time::Duration;

use belay_common::format_duration;
use belay_json::{json, FromJson, FromJsonError, ObjectReader, Value};
use belay_proto::wire::ReaderExt;

use crate::context::JobContext;
use crate::handler::{Handler, HandlerError, Outcome};

/// Returns its payload as the result.
#[derive(Clone, Copy, Debug, Default)]
pub struct EchoHandler;

impl Handler for EchoHandler {
    fn handle(&self, ctx: &mut JobContext) -> Result<Outcome, HandlerError> {
        Ok(Outcome::with(ctx.payload().clone()))
    }
}

/// Sleeps cooperatively, reporting progress, then succeeds with
/// `{"slept": "<duration>"}`.
///
/// Payload: `{"for": "1.5s"}` (any duration string; absent or `null`
/// means the handler's default). Stops early with
/// [`HandlerError::cancelled`] when the job is cancelled.
#[derive(Clone, Copy, Debug)]
pub struct SleepHandler {
    default: Duration,
    max: Duration,
}

#[derive(Debug)]
struct SleepPayload {
    duration: Option<Duration>,
}

impl FromJson for SleepPayload {
    fn from_json(value: &Value) -> Result<SleepPayload, FromJsonError> {
        if value.is_null() {
            return Ok(SleepPayload { duration: None });
        }
        let r = ObjectReader::new(value, "SleepPayload")?;
        let payload = SleepPayload {
            duration: r.opt_duration("for")?,
        };
        r.deny_unknown_fields()?;
        Ok(payload)
    }
}

impl SleepHandler {
    /// Sleeps one second by default and at most an hour.
    #[must_use]
    pub fn new() -> SleepHandler {
        SleepHandler {
            default: Duration::from_secs(1),
            max: Duration::from_secs(3600),
        }
    }

    /// Sets the duration used when the payload has none.
    #[must_use]
    pub fn default_duration(mut self, d: Duration) -> SleepHandler {
        self.default = d;
        self
    }

    /// Rejects payloads asking for longer than `max`.
    #[must_use]
    pub fn max(mut self, max: Duration) -> SleepHandler {
        self.max = max;
        self
    }
}
