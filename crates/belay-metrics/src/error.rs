//! The error type shared by every fallible operation in the crate.

use std::fmt;

/// Errors from defining metrics, registering them, or resolving labelled
/// children of a vec.
///
/// Updating an existing handle never fails; every error here is raised while
/// setting metrics up or looking a child up by its labels.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// A metric name does not match `[a-zA-Z_:][a-zA-Z0-9_:]*`.
    InvalidMetricName(String),
    /// A label name is malformed or reserved.
    InvalidLabelName {
        /// The offending label name.
        name: String,
        /// Which rule it breaks.
        reason: &'static str,
    },
    /// The same label name was declared or supplied more than once.
    DuplicateLabel(String),
    /// A child was requested with the wrong number of label values.
    LabelCount {
        /// Number of label names the family declares.
        expected: usize,
        /// Number of values supplied.
        got: usize,
    },
    /// A `(name, value)` pair named a label the family does not declare.
    UnknownLabel(String),
    /// A `(name, value)` list left out one of the family's labels.
    MissingLabel(String),
    /// Histogram bucket bounds (or a bucket generator's arguments) are unusable.
    InvalidBuckets(String),
    /// Summary options are unusable.
    InvalidSummary(String),
    /// The name is already registered with a different definition.
    Conflict {
        /// The metric name being registered.
        name: String,
        /// How the existing definition differs.
        detail: String,
    },
    /// The family would expose a series name that another family already
    /// exposes (for example a gauge `rpc_count` next to a histogram `rpc`).
    Collision {
        /// The metric name being registered.
        name: String,
        /// The series name both families would write.
        series: String,
        /// The already-registered family that owns `series`.
        owner: String,
    },
}

/// Shorthand for results carrying this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidMetricName(name) => write!(
                f,
                "invalid metric name {name:?}: must match [a-zA-Z_:][a-zA-Z0-9_:]*"
            ),
            Error::InvalidLabelName { name, reason } => {
                write!(f, "invalid label name {name:?}: {reason}")
            }
            Error::DuplicateLabel(name) => write!(f, "label {name:?} given more than once"),
            Error::LabelCount { expected, got } => write!(
                f,
                "expected {expected} label value{}, got {got}",
                if *expected == 1 { "" } else { "s" }
            ),
            Error::UnknownLabel(name) => write!(f, "unknown label {name:?}"),
            Error::MissingLabel(name) => write!(f, "missing value for label {name:?}"),
            Error::InvalidBuckets(detail) => write!(f, "invalid histogram buckets: {detail}"),
            Error::InvalidSummary(detail) => write!(f, "invalid summary options: {detail}"),
            Error::Conflict { name, detail } => write!(f, "metric {name:?} {detail}"),
            Error::Collision {
                name,
                series,
                owner,
            } => write!(
                f,
                "metric {name:?} would expose series {series:?}, which clashes with metric {owner:?}"
            ),
        }
    }
}
