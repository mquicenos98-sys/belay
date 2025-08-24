//! The crate's error type, and its mapping onto API errors so that the
//! server can answer with the right status without knowing our variants.

use std::fmt;

use belay_json::json;
use belay_proto::api::ApiError;
use belay_proto::validate::Violations;
use belay_proto::ScheduleId;
use belay_store::StoreError;

use crate::dispatch::DispatchError;

/// Anything that can go wrong in the scheduler or the workflow engine.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// A schedule, calendar or workflow failed validation. Every problem is
    /// listed, each with the path of the offending field.
    Invalid(Violations),
    /// The named schedule, calendar or workflow does not exist.
    NotFound {
        /// `"schedule"`, `"calendar"` or `"workflow"`.
        kind: &'static str,
        /// The id or name that was asked for.
        id: String,
    },
    /// Creating something that already exists.
    AlreadyExists {
        /// `"schedule"` or `"calendar"`.
        kind: &'static str,
        /// The id or name.
        id: String,
    },
    /// A calendar cannot be deleted while schedules reference it.
    CalendarInUse {
        /// The calendar.
        calendar: String,
        /// The schedules that use it, sorted.
        schedules: Vec<ScheduleId>,
    },
    /// The dispatcher refused or could not take a job.
    Dispatch(DispatchError),
    /// The store failed.
    Store(StoreError),
    /// A stored document could not be decoded.
    Corrupt {
        /// The store key, printable.
        key: String,
        /// What was wrong with it.
        reason: String,
    },
    /// Engine options are out of range.
    InvalidOptions(String),
}

/// `Result` defaulting to this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;
