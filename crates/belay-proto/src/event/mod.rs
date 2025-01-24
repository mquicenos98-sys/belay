//! The event stream: every notable change in the system, numbered.
//!
//! Engines report what happens through an [`EventSink`](crate::EventSink)
//! as bare [`EventKind`]s; the event bus in belay-core stamps each with a
//! sequence number and time, producing an [`Event`]. Subscribers
//! (`GET /v1/events`, server-sent events) resume from a sequence number.
//!
//! # Wire form
//!
//! One flat object: `seq`, `at`, `type`, then the fields of the kind.
//!
//! ```json
//! {"seq":42,"at":"2025-03-04T05:06:07.089Z","type":"job.succeeded",
//!  "job":"01HQ3K9Z8N6V2T4R5S7W0X1Y2Z","queue":"emails","attempt":1,"runtime":"1s250ms"}
//! ```
//!
//! Type names are `<category>.<what>` and stable; see [`EventKind::TYPES`].
//! Decoding ignores fields it does not know, so fields can be added to a
//! kind without breaking older clients; a new *type* is not readable by an
//! older client, which should skip events it cannot decode.

mod codec;

mod filter;

pub use filter::{EventFilter, EventPattern};

use std::time::Duration;

use belay_common::Timestamp;

use crate::ids::{JobId, QueueName, ScheduleId, WorkerId, WorkflowId};
use crate::job::{JobState, Progress};
use crate::schedule::MisfirePolicy;
use crate::wire::text_enum;
use crate::workflow::{StepState, WorkflowState};

/// An event as delivered to subscribers.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// Position in the stream, increasing by one per event.
    pub seq: u64,
    /// When it happened.
    pub at: Timestamp,
    /// What happened.
    pub kind: EventKind,
}

impl Event {
    /// The event's type name, e.g. `job.succeeded`.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        self.kind.type_name()
    }
}

text_enum! {
    /// Why a schedule did not fire at one of its times.
    SkipReason, "skip reason" {
        /// The previous job was still running under the `skip` overlap policy.
        Overlap = "overlap",
        /// The time fell outside the schedule's calendar.
        Calendar = "calendar",
    }
}
