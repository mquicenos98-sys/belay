//! What can go wrong talking to a belay server, and how to tell the cases
//! apart: the server said no ([`Error::Api`]), the conversation failed
//! ([`Error::Transport`]), the answer made no sense ([`Error::Decode`]), or
//! the client was set up wrongly ([`Error::Config`]).

use std::fmt;
use std::io;

use belay_proto::api::{ApiError, ErrorCode};

/// Errors returned by the client.
#[derive(Debug)]
pub enum Error {
    /// The server answered with an error, decoded from its body (or
    /// reconstructed from the status when there was no body, e.g. from a
    /// proxy). Branch on [`ApiError::code`], never on the message.
    Api(ApiError),
    /// The request did not get an answer: connection refused or reset, a
    /// timeout, a malformed response, too many redirects.
    Transport(belay_http::Error),
    /// The server answered successfully, but the body is not what this
    /// endpoint returns. Usually a version mismatch or a proxy answering
    /// in place of belay.
    Decode(DecodeError),
    /// The client configuration is unusable (bad base URL, bad token).
    Config(String),
}

/// `Result` with the client's [`Error`] as the default error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// The server's error, if it answered with one.
    #[must_use]
    pub fn api_error(&self) -> Option<&ApiError> {
        match self {
            Error::Api(e) => Some(e),
            _ => None,
        }
    }

    /// The server's error code, if it answered with an error.
    #[must_use]
    pub fn code(&self) -> Option<ErrorCode> {
        self.api_error().map(|e| e.code)
    }

    /// Whether the server said the resource does not exist.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        self.code() == Some(ErrorCode::NotFound)
    }

    /// Whether a worker's lease is gone (expired or handed to another
    /// worker); the job must be abandoned without further reports.
    #[must_use]
    pub fn is_lease_lost(&self) -> bool {
        self.code() == Some(ErrorCode::LeaseLost)
    }

    /// Whether the same call may succeed later: the server was unavailable
    /// or rate limited, or the connection failed or timed out. The client
    /// has already retried what it safely could by the time this is seen;
    /// retrying a non-idempotent call is the caller's decision.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Api(e) => e.is_retryable(),
            Error::Transport(e) => e.is_transient(),
            Error::Decode(_) | Error::Config(_) => false,
        }
    }

    /// Whether the request timed out waiting for the server.
    #[must_use]
    pub fn is_timeout(&self) -> bool {
        matches!(self, Error::Transport(e) if e.is_timeout())
    }

    /// Converts to the [`ApiError`] shape used by
    /// [`JobSource`](belay_proto::JobSource), so a worker handles remote
    /// and embedded sources alike. Connection failures and timeouts become
    /// `unavailable` (retryable), undecodable answers `internal`.
    #[must_use]
    pub fn into_api_error(self) -> ApiError {
        match self {
            Error::Api(e) => e,
            Error::Transport(e) if e.is_transient() => {
                ApiError::unavailable(format!("belay server unreachable: {e}"))
            }
            Error::Transport(e) => ApiError::internal(format!("request failed: {e}")),
            Error::Decode(e) => ApiError::internal(e.to_string()),
            Error::Config(message) => ApiError::invalid_argument(message),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Api(e) => write!(f, "server error: {e}"),
            Error::Transport(e) => write!(f, "request failed: {e}"),
            Error::Decode(e) => e.fmt(f),
            Error::Config(message) => write!(f, "invalid client configuration: {message}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Api(e) => Some(e),
            Error::Transport(e) => Some(e),
            Error::Decode(e) => Some(e),
            Error::Config(_) => None,
        }
    }
}

impl From<ApiError> for Error {
    fn from(e: ApiError) -> Error {
        Error::Api(e)
    }
}

impl From<belay_http::Error> for Error {
    fn from(e: belay_http::Error) -> Error {
        Error::Transport(e)
    }
}

impl From<DecodeError> for Error {
    fn from(e: DecodeError) -> Error {
        Error::Decode(e)
    }
}

impl From<Error> for ApiError {
    fn from(e: Error) -> ApiError {
        e.into_api_error()
    }
}

/// A successful response whose body could not be read as the expected
/// type.
#[derive(Debug)]
pub struct DecodeError {
    endpoint: String,
    message: String,
    source: Option<belay_json::Error>,
}

impl DecodeError {
    /// A decoding failure of the response to `endpoint` (`GET /v1/info`).
    pub(crate) fn new(endpoint: impl Into<String>, message: impl Into<String>) -> DecodeError {
        DecodeError {
            endpoint: endpoint.into(),
            message: message.into(),
            source: None,
        }
    }

    /// A JSON parse or shape error in the response to `endpoint`.
    pub(crate) fn json(endpoint: impl Into<String>, source: belay_json::Error) -> DecodeError {
        DecodeError {
            endpoint: endpoint.into(),
            message: source.to_string(),
            source: Some(source),
        }
    }

    /// The request whose response failed to decode, e.g. `GET /v1/info`.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// What was wrong with the body.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unexpected response to {}: {}",
            self.endpoint, self.message
        )
    }
}

impl std::error::Error for DecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|e| e as &(dyn std::error::Error + 'static))
    }
}

/// Whether a transport failure proves the request never reached the
/// server, which makes repeating it harmless whatever it does: the TCP
/// connection was refused or never completed, so not one byte was written.
pub(crate) fn never_sent(e: &belay_http::Error) -> bool {
    match e {
        belay_http::Error::Timeout(phase) => *phase == "connect",
        belay_http::Error::Io(e) => matches!(
            e.kind(),
            io::ErrorKind::ConnectionRefused | io::ErrorKind::AddrNotAvailable
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    fn io(kind: io::ErrorKind) -> belay_http::Error {
        belay_http::Error::Io(io::Error::from(kind))
    }

    #[test]
    fn helpers_classify_api_errors() {
        let err = Error::Api(ApiError::not_found("job", "01HQ3K9Z8N6V2T4R5S7W0X1Y2Z"));
        assert!(err.is_not_found());
        assert!(!err.is_retryable());
        assert_eq!(err.code(), Some(ErrorCode::NotFound));
        assert_eq!(
            err.to_string(),
            "server error: not_found: job 01HQ3K9Z8N6V2T4R5S7W0X1Y2Z not found"
        );
        let err = Error::Api(ApiError::unavailable("draining"));
        assert!(err.is_retryable() && !err.is_not_found());
        let err = Error::Api(ApiError::new(ErrorCode::LeaseLost, "gone"));
        assert!(err.is_lease_lost());
    }

    #[test]
    fn transport_errors_are_retryable_only_when_transient() {
        assert!(Error::Transport(io(io::ErrorKind::ConnectionReset)).is_retryable());
        let timeout = Error::Transport(belay_http::Error::Timeout("read"));
        assert!(timeout.is_retryable() && timeout.is_timeout());
        let redirects = Error::Transport(belay_http::Error::TooManyRedirects { max: 5 });
        assert!(!redirects.is_retryable());
        assert!(redirects.api_error().is_none() && redirects.code().is_none());
    }

    #[test]
    fn only_refused_or_unfinished_connects_count_as_never_sent() {
        assert!(never_sent(&io(io::ErrorKind::ConnectionRefused)));
        assert!(never_sent(&belay_http::Error::Timeout("connect")));
        assert!(!never_sent(&belay_http::Error::Timeout("read")));
        assert!(!never_sent(&belay_http::Error::Timeout("write")));
        assert!(!never_sent(&io(io::ErrorKind::ConnectionReset)));
        assert!(!never_sent(&belay_http::Error::ConnectionClosed));
    }

    #[test]
    fn job_source_mapping_keeps_codes_and_marks_outages_retryable() {
        let lost = ApiError::new(ErrorCode::LeaseLost, "gone");
        assert_eq!(Error::Api(lost.clone()).into_api_error(), lost);
        let down = Error::Transport(io(io::ErrorKind::ConnectionRefused)).into_api_error();
        assert_eq!(down.code, ErrorCode::Unavailable);
        assert!(
            down.message.starts_with("belay server unreachable: "),
            "{}",
            down.message
        );
        let decode = Error::Decode(DecodeError::new("GET /v1/info", "empty body"));
        let mapped = ApiError::from(decode);
        assert_eq!(mapped.code, ErrorCode::Internal);
        assert_eq!(
            mapped.message,
            "unexpected response to GET /v1/info: empty body"
        );
    }

    #[test]
    fn decode_errors_keep_their_json_source() {
        let json_err = belay_json::from_str::<u8>("[").unwrap_err();
        let err = Error::Decode(DecodeError::json("POST /v1/jobs", json_err));
        assert!(err
            .to_string()
            .starts_with("unexpected response to POST /v1/jobs: invalid JSON"));
        let decode = err.source().expect("decode error is the source");
        assert!(decode.source().is_some(), "the JSON error is kept");
        match &err {
            Error::Decode(d) => assert_eq!(d.endpoint(), "POST /v1/jobs"),
            other => panic!("unexpected {other:?}"),
        }
    }
}
