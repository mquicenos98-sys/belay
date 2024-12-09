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
