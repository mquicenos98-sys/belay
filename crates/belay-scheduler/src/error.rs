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

impl Error {
    pub(crate) fn not_found(kind: &'static str, id: impl fmt::Display) -> Error {
        Error::NotFound {
            kind,
            id: id.to_string(),
        }
    }

    pub(crate) fn already_exists(kind: &'static str, id: impl fmt::Display) -> Error {
        Error::AlreadyExists {
            kind,
            id: id.to_string(),
        }
    }

    /// Whether this is a [`NotFound`](Error::NotFound) error.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(self, Error::NotFound { .. })
    }

    /// The validation problems, for [`Invalid`](Error::Invalid) errors.
    #[must_use]
    pub fn violations(&self) -> Option<&Violations> {
        match self {
            Error::Invalid(v) => Some(v),
            _ => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Invalid(v) => v.fmt(f),
            Error::NotFound { kind, id } => write!(f, "{kind} {id:?} not found"),
            Error::AlreadyExists { kind, id } => write!(f, "{kind} {id:?} already exists"),
            Error::CalendarInUse {
                calendar,
                schedules,
            } => {
                let names: Vec<&str> = schedules.iter().map(ScheduleId::as_str).collect();
                write!(
                    f,
                    "calendar {calendar:?} is used by {} ({})",
                    belay_common::text::plural(schedules.len() as u64, "schedule"),
                    names.join(", ")
                )
            }
            Error::Dispatch(e) => write!(f, "dispatching the job failed: {e}"),
            Error::Store(e) => write!(f, "storage failed: {e}"),
            Error::Corrupt { key, reason } => {
                write!(f, "stored record {key} is unreadable: {reason}")
            }
            Error::InvalidOptions(reason) => write!(f, "invalid options: {reason}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Invalid(v) => Some(v),
            Error::Dispatch(e) => Some(e),
            Error::Store(e) => Some(e),
            _ => None,
        }
    }
}

impl From<Violations> for Error {
    fn from(v: Violations) -> Error {
        Error::Invalid(v)
    }
}

impl From<StoreError> for Error {
    fn from(e: StoreError) -> Error {
        Error::Store(e)
    }
}

impl From<DispatchError> for Error {
    fn from(e: DispatchError) -> Error {
        Error::Dispatch(e)
    }
}

/// How the HTTP API reports each error: validation problems as
/// `invalid_argument` with the violation list, missing things as
/// `not_found`, a calendar in use as `failed_precondition`, a busy
/// dispatcher as `unavailable`, and storage trouble as `internal`.
impl From<Error> for ApiError {
    fn from(e: Error) -> ApiError {
        match e {
            Error::Invalid(v) => ApiError::from(v),
            Error::NotFound { kind, id } => ApiError::not_found(kind, id),
            Error::AlreadyExists { kind, id } => ApiError::already_exists(kind, id),
            Error::CalendarInUse {
                ref calendar,
                ref schedules,
            } => {
                let names: Vec<&str> = schedules.iter().map(ScheduleId::as_str).collect();
                let details = json!({"calendar": calendar.as_str(), "schedules": names});
                ApiError::failed_precondition(e.to_string()).with_details(details)
            }
            Error::Dispatch(DispatchError::Unavailable(_)) => ApiError::unavailable(e.to_string()),
            Error::Dispatch(DispatchError::Rejected(_)) => {
                ApiError::failed_precondition(e.to_string())
            }
            Error::Store(_) | Error::Corrupt { .. } | Error::InvalidOptions(_) => {
                ApiError::internal(e.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_json::PathSegment;
    use belay_proto::api::ErrorCode;

    #[test]
    fn messages_name_the_thing_concerned() {
        assert_eq!(
            Error::not_found("schedule", "nightly").to_string(),
            r#"schedule "nightly" not found"#
        );
        assert_eq!(
            Error::already_exists("calendar", "holidays").to_string(),
            r#"calendar "holidays" already exists"#
        );
        let in_use = Error::CalendarInUse {
            calendar: "workdays".into(),
            schedules: vec![
                ScheduleId::from_static("billing"),
                ScheduleId::from_static("reports"),
            ],
        };
        assert_eq!(
            in_use.to_string(),
            r#"calendar "workdays" is used by 2 schedules (billing, reports)"#
        );
        let invalid = Error::from(Violations::single(
            &[PathSegment::Key("cron".into())],
            "must not be empty",
        ));
        assert_eq!(invalid.to_string(), "$.cron: must not be empty");
        assert!(invalid.violations().is_some());
    }

    #[test]
    fn api_errors_carry_the_right_codes() {
        let cases: Vec<(Error, ErrorCode)> = vec![
            (Error::not_found("workflow", "x"), ErrorCode::NotFound),
            (
                Error::already_exists("schedule", "x"),
                ErrorCode::AlreadyExists,
            ),
            (
                Error::CalendarInUse {
                    calendar: "c".into(),
                    schedules: vec![ScheduleId::from_static("s")],
                },
                ErrorCode::FailedPrecondition,
            ),
            (
                Error::Dispatch(DispatchError::Unavailable("draining".into())),
                ErrorCode::Unavailable,
            ),
            (
                Error::Dispatch(DispatchError::Rejected("bad queue".into())),
                ErrorCode::FailedPrecondition,
            ),
            (
                Error::Corrupt {
                    key: "sched/x".into(),
                    reason: "not JSON".into(),
                },
                ErrorCode::Internal,
            ),
        ];
        for (error, code) in cases {
            let text = error.to_string();
            let api = ApiError::from(error);
            assert_eq!(api.code, code, "{text}");
        }
        let api = ApiError::from(Error::CalendarInUse {
            calendar: "c".into(),
            schedules: vec![ScheduleId::from_static("s")],
        });
        assert_eq!(api.details.unwrap()["schedules"][0], "s");
    }
}
