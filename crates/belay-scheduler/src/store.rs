//! Where the engines keep their records, and the JSON encoding of them.
//!
//! Keys are readable text so that `belay admin store-inspect` output makes
//! sense without a decoder:
//!
//! | key | value |
//! |---|---|
//! | `sched/<schedule id>` | [`Schedule`](belay_proto::Schedule) |
//! | `cal/<calendar name>` | [`CalendarSpec`](belay_proto::CalendarSpec) |
//! | `wf/<run id>` | [`WorkflowDetails`](crate::WorkflowDetails) |
//! | `wfr/<run id>/<step>` | the result of a step that a later step's placeholders read |
//!
//! Schedule ids, calendar names and step names cannot contain `/`, and run
//! ids are ULIDs, whose text form sorts in creation order, so a prefix scan
//! of `wf/` visits runs oldest first. None of the prefixes is a prefix of
//! another.

use belay_json::{FromJson, ToJson};
use belay_proto::{ScheduleId, WorkflowId};
use belay_store::{KeyRange, Kv};

use crate::error::{Error, Result};

const SCHEDULES: &str = "sched/";

const CALENDARS: &str = "cal/";

const WORKFLOWS: &str = "wf/";

const RESULTS: &str = "wfr/";

pub(crate) fn schedule_key(id: &ScheduleId) -> Vec<u8> {
    format!("{SCHEDULES}{id}").into_bytes()
}

pub(crate) fn schedules() -> KeyRange {
    KeyRange::prefix(SCHEDULES)
}
