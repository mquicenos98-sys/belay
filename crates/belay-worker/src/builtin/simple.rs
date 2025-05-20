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
