//! The error every endpoint returns: [`ApiError`], serialized as
//! `{"error": {"code": "not_found", "message": "...", "details": ...}}`.

use std::fmt;

use belay_json::{json, FromJson, FromJsonError, ObjectBuilder, ObjectReader, ToJson, Value};

use crate::error::{Error, ParseError};
use crate::ids::JobId;
use crate::job::TransitionError;
use crate::validate::Violations;
use crate::wire::text_enum;

text_enum! {
    /// Machine-readable error class. Clients branch on this, never on the
    /// message.
    ///
    /// | code | HTTP |
    /// |---|---|
    /// | `invalid_argument` | 400 |
    /// | `unauthenticated` | 401 |
    /// | `permission_denied` | 403 |
    /// | `not_found` | 404 |
    /// | `already_exists`, `conflict`, `lease_lost`, `failed_precondition` | 409 |
    /// | `resource_exhausted` | 429 |
    /// | `internal` | 500 |
    /// | `unavailable` | 503 |
    #[non_exhaustive]
    ErrorCode, "error code" {
        /// The request is malformed or breaks a rule; do not retry as is.
        InvalidArgument = "invalid_argument",
        /// The named resource does not exist.
        NotFound = "not_found",
        /// Creating something that already exists.
        AlreadyExists = "already_exists",
        /// Concurrent modification or a state conflict.
        Conflict = "conflict",
        /// The lease token is stale: the lease expired or the job was
        /// handed to another worker. The worker must abandon the job.
        LeaseLost = "lease_lost",
        /// The resource is not in a state that allows the operation, such
        /// as deleting a running job.
        FailedPrecondition = "failed_precondition",
        /// No valid credentials.
        Unauthenticated = "unauthenticated",
        /// Credentials lack the required scope.
        PermissionDenied = "permission_denied",
        /// A rate limit or quota was hit; retry later.
        ResourceExhausted = "resource_exhausted",
        /// The server cannot serve the request right now; retry later.
        Unavailable = "unavailable",
        /// A bug or unexpected failure on the server.
        Internal = "internal",
    }
}

impl ErrorCode {
    /// The HTTP status the server answers with.
    #[must_use]
    pub const fn http_status(self) -> u16 {
        match self {
            ErrorCode::InvalidArgument => 400,
            ErrorCode::Unauthenticated => 401,
            ErrorCode::PermissionDenied => 403,
            ErrorCode::NotFound => 404,
            ErrorCode::AlreadyExists
            | ErrorCode::Conflict
            | ErrorCode::LeaseLost
            | ErrorCode::FailedPrecondition => 409,
            ErrorCode::ResourceExhausted => 429,
            ErrorCode::Internal => 500,
            ErrorCode::Unavailable => 503,
        }
    }

    /// The best guess for a response that carried no error body (a proxy's
    /// 502, say).
    #[must_use]
    pub const fn from_http_status(status: u16) -> ErrorCode {
        match status {
            401 => ErrorCode::Unauthenticated,
            403 => ErrorCode::PermissionDenied,
            404 => ErrorCode::NotFound,
            409 => ErrorCode::Conflict,
            412 => ErrorCode::FailedPrecondition,
            429 => ErrorCode::ResourceExhausted,
            502..=504 => ErrorCode::Unavailable,
            400..=499 => ErrorCode::InvalidArgument,
            _ => ErrorCode::Internal,
        }
    }

    /// Whether the same request may succeed if sent again later.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(self, ErrorCode::Unavailable | ErrorCode::ResourceExhausted)
    }
}

/// An API failure.
///
/// ```
/// use belay_json::ToJson;
/// use belay_proto::api::{ApiError, ErrorCode};
///
/// let err = ApiError::not_found("queue", "emails");
/// assert_eq!(err.http_status(), 404);
/// assert_eq!(
///     err.to_json().to_string(),
///     r#"{"error":{"code":"not_found","message":"queue emails not found","details":{"resource":"queue","id":"emails"}}}"#
/// );
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct ApiError {
    /// The error class.
    pub code: ErrorCode,
    /// A human-readable explanation.
    pub message: String,
    /// Structured context, e.g. `{"violations": [...]}` for invalid input.
    pub details: Option<Value>,
}

impl ApiError {
    /// An error without details.
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> ApiError {
        ApiError {
            code,
            message: message.into(),
            details: None,
        }
    }

    /// Attaches details.
    #[must_use]
    pub fn with_details(mut self, details: Value) -> ApiError {
        self.details = Some(details);
        self
    }

    /// `invalid_argument`.
    #[must_use]
    pub fn invalid_argument(message: impl Into<String>) -> ApiError {
        ApiError::new(ErrorCode::InvalidArgument, message)
    }

    /// `not_found` for the `resource` (`"job"`, `"queue"`, ...) named `id`.
    #[must_use]
    pub fn not_found(resource: &str, id: impl fmt::Display) -> ApiError {
        let id = id.to_string();
        ApiError::new(ErrorCode::NotFound, format!("{resource} {id} not found"))
            .with_details(json!({"resource": resource, "id": id}))
    }

    /// `already_exists` for the `resource` named `id`.
    #[must_use]
    pub fn already_exists(resource: &str, id: impl fmt::Display) -> ApiError {
        let id = id.to_string();
        ApiError::new(
            ErrorCode::AlreadyExists,
            format!("{resource} {id} already exists"),
        )
        .with_details(json!({"resource": resource, "id": id}))
    }

    /// `conflict`.
    #[must_use]
    pub fn conflict(message: impl Into<String>) -> ApiError {
        ApiError::new(ErrorCode::Conflict, message)
    }

    /// `lease_lost` for `job`.
    #[must_use]
    pub fn lease_lost(job: JobId) -> ApiError {
        ApiError::new(
            ErrorCode::LeaseLost,
            format!("lease on job {job} is no longer held (expired or given to another worker)"),
        )
        .with_details(json!({"job": job}))
    }

    /// `failed_precondition`.
    #[must_use]
    pub fn failed_precondition(message: impl Into<String>) -> ApiError {
        ApiError::new(ErrorCode::FailedPrecondition, message)
    }

    /// `unauthenticated`.
    #[must_use]
    pub fn unauthenticated(message: impl Into<String>) -> ApiError {
        ApiError::new(ErrorCode::Unauthenticated, message)
    }

    /// `permission_denied`.
    #[must_use]
    pub fn permission_denied(message: impl Into<String>) -> ApiError {
        ApiError::new(ErrorCode::PermissionDenied, message)
    }

    /// `resource_exhausted`.
    #[must_use]
    pub fn resource_exhausted(message: impl Into<String>) -> ApiError {
        ApiError::new(ErrorCode::ResourceExhausted, message)
    }

    /// `unavailable`.
    #[must_use]
    pub fn unavailable(message: impl Into<String>) -> ApiError {
        ApiError::new(ErrorCode::Unavailable, message)
    }

    /// `internal`.
    #[must_use]
    pub fn internal(message: impl Into<String>) -> ApiError {
        ApiError::new(ErrorCode::Internal, message)
    }

    /// For a response without a decodable error body: the code is guessed
    /// from the status and the (truncated) body becomes the message.
    #[must_use]
    pub fn from_http_status(status: u16, body: &str) -> ApiError {
        const MAX_BODY_CHARS: usize = 200;
        let body: String = body.trim().chars().take(MAX_BODY_CHARS).collect();
        let message = if body.is_empty() {
            format!("HTTP {status}")
        } else {
            format!("HTTP {status}: {body}")
        };
        ApiError::new(ErrorCode::from_http_status(status), message)
    }

    /// The HTTP status for this error.
    #[must_use]
    pub fn http_status(&self) -> u16 {
        self.code.http_status()
    }

    /// Whether this is a `code` error.
    #[must_use]
    pub fn is(&self, code: ErrorCode) -> bool {
        self.code == code
    }

    /// See [`ErrorCode::is_retryable`].
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        self.code.is_retryable()
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

impl ToJson for ApiError {
    fn to_json(&self) -> Value {
        let inner = ObjectBuilder::new()
            .field("code", self.code)
            .field("message", &self.message)
            .opt_field("details", &self.details)
            .build();
        ObjectBuilder::new().raw("error", inner).build()
    }
}

impl FromJson for ApiError {
    /// Tolerant, since it decodes whatever a server sent: an error code this
    /// version does not know becomes `internal` with the code kept in the
    /// message.
    fn from_json(value: &Value) -> Result<ApiError, FromJsonError> {
        let outer = ObjectReader::new(value, "ApiError")?;
        outer.req_with("error", |inner| {
            let r = ObjectReader::new(inner, "ApiError")?;
            let raw_code = r.req_str("code")?;
            let mut message: String = r.or_default("message")?;
            let code = raw_code.parse().unwrap_or_else(|_| {
                message = format!("{raw_code}: {message}");
                ErrorCode::Internal
            });
            Ok(ApiError {
                code,
                message,
                details: r.value("details").filter(|d| !d.is_null()).cloned(),
            })
        })
    }
}

/// Invalid input: `invalid_argument` listing every problem under
/// `details.violations` as `{"field": "$.path", "message": "..."}`.
impl From<Violations> for ApiError {
    fn from(v: Violations) -> ApiError {
        let list: Vec<Value> = v
            .iter()
            .map(|x| json!({"field": x.path(), "message": x.message()}))
            .collect();
        ApiError::invalid_argument(v.to_string()).with_details(json!({ "violations": list }))
    }
}

impl From<FromJsonError> for ApiError {
    fn from(e: FromJsonError) -> ApiError {
        Violations::from(e).into()
    }
}

impl From<ParseError> for ApiError {
    fn from(e: ParseError) -> ApiError {
        ApiError::invalid_argument(e.to_string())
    }
}

impl From<TransitionError> for ApiError {
    fn from(e: TransitionError) -> ApiError {
        ApiError::failed_precondition(e.to_string())
            .with_details(json!({"state": e.from, "requested": e.to}))
    }
}

impl From<belay_json::Error> for ApiError {
    fn from(e: belay_json::Error) -> ApiError {
        match e {
            belay_json::Error::Convert(e) => e.into(),
            belay_json::Error::Parse(e) => ApiError::invalid_argument(format!("invalid JSON: {e}"))
                .with_details(json!({"line": e.line(), "column": e.column()})),
            other => ApiError::internal(other.to_string()),
        }
    }
}

impl From<Error> for ApiError {
    fn from(e: Error) -> ApiError {
        match e {
            Error::Parse(e) => e.into(),
            Error::Json(e) => e.into(),
            Error::Invalid(e) => e.into(),
            Error::Transition(e) => e.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{JobSpec, JobState};
    use crate::validate::Validate;

    #[test]
    fn every_code_has_a_status_and_round_trips() {
        for code in ErrorCode::ALL {
            let status = code.http_status();
            assert!((400..=599).contains(&status), "{code}");
            let err = ApiError::new(*code, "m").with_details(json!([1]));
            assert_eq!(ApiError::from_json(&err.to_json()), Ok(err));
        }
        assert_eq!(ErrorCode::LeaseLost.http_status(), 409);
    }

    #[test]
    fn status_mapping_for_bodiless_responses() {
        let cases = [
            (400, ErrorCode::InvalidArgument),
            (401, ErrorCode::Unauthenticated),
            (404, ErrorCode::NotFound),
            (413, ErrorCode::InvalidArgument),
            (429, ErrorCode::ResourceExhausted),
            (500, ErrorCode::Internal),
            (502, ErrorCode::Unavailable),
            (504, ErrorCode::Unavailable),
        ];
        for (status, code) in cases {
            assert_eq!(ErrorCode::from_http_status(status), code, "{status}");
        }
        let err = ApiError::from_http_status(502, "  <html>Bad Gateway</html> ");
        assert_eq!(
            err.to_string(),
            "unavailable: HTTP 502: <html>Bad Gateway</html>"
        );
        assert!(err.is_retryable());
        assert_eq!(ApiError::from_http_status(500, "").message, "HTTP 500");
    }

    #[test]
    fn constructors_carry_useful_details() {
        let id: JobId = "01HQ3K9Z8N6V2T4R5S7W0X1Y2Z".parse().unwrap();
        let err = ApiError::lease_lost(id);
        assert!(err.is(ErrorCode::LeaseLost));
        assert_eq!(
            err.details,
            Some(json!({"job": "01HQ3K9Z8N6V2T4R5S7W0X1Y2Z"}))
        );
        let err = ApiError::already_exists("schedule", "nightly");
        assert_eq!(
            err.to_string(),
            "already_exists: schedule nightly already exists"
        );
        let err: ApiError = JobState::Succeeded
            .check_transition(JobState::Cancelled)
            .unwrap_err()
            .into();
        assert_eq!(err.code, ErrorCode::FailedPrecondition);
        assert_eq!(
            err.details,
            Some(json!({"state": "succeeded", "requested": "cancelled"}))
        );
    }

    #[test]
    fn violations_become_invalid_argument_with_fields() {
        let problems = JobSpec::new("q", "k")
            .priority(3000)
            .max_attempts(0)
            .validate()
            .unwrap_err();
        let err = ApiError::from(problems);
        assert_eq!(err.http_status(), 400);
        assert_eq!(
            err.message,
            "$.priority: must be between -1000 and 1000, got 3000 (and 1 more problem)"
        );
        assert_eq!(
            err.details,
            Some(json!({"violations": [
                {"field": "$.priority", "message": "must be between -1000 and 1000, got 3000"},
                {"field": "$.max_attempts", "message": "must be between 1 and 1000, got 0"}
            ]}))
        );
    }

    #[test]
    fn json_errors_map_to_invalid_argument() {
        let err: ApiError = belay_json::from_str::<JobSpec>("{").unwrap_err().into();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert!(err.message.starts_with("invalid JSON: "), "{}", err.message);
        assert_eq!(
            err.details.as_ref().map(|d| d["line"].clone()),
            Some(json!(1))
        );
        let err: ApiError = belay_json::from_str::<JobSpec>(r#"{"queue": "q"}"#)
            .unwrap_err()
            .into();
        assert_eq!(
            err.message,
            r#"$: missing required field "kind" in JobSpec"#
        );
    }

    #[test]
    fn unknown_codes_from_newer_servers_degrade_to_internal() {
        let v = json!({"error": {"code": "quota_frozen", "message": "try tomorrow"}});
        let err = ApiError::from_json(&v).unwrap();
        assert_eq!(err.code, ErrorCode::Internal);
        assert_eq!(err.message, "quota_frozen: try tomorrow");
        let err = ApiError::from_json(&json!({"message": "x"})).unwrap_err();
        assert_eq!(
            err.to_string(),
            r#"$: missing required field "error" in ApiError"#
        );
    }
}
