//! Status codes (RFC 9110 §15) and their canonical reason phrases.

use std::fmt;

use crate::error::Error;

/// An HTTP status code, guaranteed to lie in `100..=999`.
///
/// Compare against the associated constants or plain integers:
///
/// ```
/// use belay_http::StatusCode;
///
/// let status = StatusCode::from_u16(404)?;
/// assert_eq!(status, StatusCode::NOT_FOUND);
/// assert_eq!(status, 404);
/// assert!(status.is_client_error());
/// assert_eq!(status.to_string(), "404 Not Found");
/// # Ok::<(), belay_http::Error>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StatusCode(u16);

macro_rules! status_codes {
    ($( ($code:expr, $name:ident, $phrase:expr); )+) => {
        impl StatusCode {
            $(
                #[doc = concat!("`", stringify!($code), " ", $phrase, "`")]
                pub const $name: StatusCode = StatusCode($code);
            )+
        }

        fn canonical_reason(code: u16) -> Option<&'static str> {
            match code {
                $( $code => Some($phrase), )+
                _ => None,
            }
        }
    };
}

status_codes! {
    (100, CONTINUE, "Continue");
    (101, SWITCHING_PROTOCOLS, "Switching Protocols");
    (200, OK, "OK");
    (201, CREATED, "Created");
    (202, ACCEPTED, "Accepted");
    (203, NON_AUTHORITATIVE_INFORMATION, "Non-Authoritative Information");
    (204, NO_CONTENT, "No Content");
    (205, RESET_CONTENT, "Reset Content");
    (206, PARTIAL_CONTENT, "Partial Content");
    (300, MULTIPLE_CHOICES, "Multiple Choices");
    (301, MOVED_PERMANENTLY, "Moved Permanently");
    (302, FOUND, "Found");
    (303, SEE_OTHER, "See Other");
    (304, NOT_MODIFIED, "Not Modified");
    (307, TEMPORARY_REDIRECT, "Temporary Redirect");
    (308, PERMANENT_REDIRECT, "Permanent Redirect");
    (400, BAD_REQUEST, "Bad Request");
    (401, UNAUTHORIZED, "Unauthorized");
    (402, PAYMENT_REQUIRED, "Payment Required");
    (403, FORBIDDEN, "Forbidden");
    (404, NOT_FOUND, "Not Found");
    (405, METHOD_NOT_ALLOWED, "Method Not Allowed");
    (406, NOT_ACCEPTABLE, "Not Acceptable");
    (407, PROXY_AUTHENTICATION_REQUIRED, "Proxy Authentication Required");
    (408, REQUEST_TIMEOUT, "Request Timeout");
    (409, CONFLICT, "Conflict");
    (410, GONE, "Gone");
    (411, LENGTH_REQUIRED, "Length Required");
    (412, PRECONDITION_FAILED, "Precondition Failed");
    (413, CONTENT_TOO_LARGE, "Content Too Large");
    (414, URI_TOO_LONG, "URI Too Long");
    (415, UNSUPPORTED_MEDIA_TYPE, "Unsupported Media Type");
    (416, RANGE_NOT_SATISFIABLE, "Range Not Satisfiable");
    (417, EXPECTATION_FAILED, "Expectation Failed");
    (421, MISDIRECTED_REQUEST, "Misdirected Request");
    (422, UNPROCESSABLE_CONTENT, "Unprocessable Content");
    (425, TOO_EARLY, "Too Early");
    (426, UPGRADE_REQUIRED, "Upgrade Required");
    (428, PRECONDITION_REQUIRED, "Precondition Required");
    (429, TOO_MANY_REQUESTS, "Too Many Requests");
    (431, REQUEST_HEADER_FIELDS_TOO_LARGE, "Request Header Fields Too Large");
    (451, UNAVAILABLE_FOR_LEGAL_REASONS, "Unavailable For Legal Reasons");
    (500, INTERNAL_SERVER_ERROR, "Internal Server Error");
    (501, NOT_IMPLEMENTED, "Not Implemented");
    (502, BAD_GATEWAY, "Bad Gateway");
    (503, SERVICE_UNAVAILABLE, "Service Unavailable");
    (504, GATEWAY_TIMEOUT, "Gateway Timeout");
    (505, HTTP_VERSION_NOT_SUPPORTED, "HTTP Version Not Supported");
}

impl StatusCode {
    /// The pre-RFC 9110 name of `413 Content Too Large`.
    pub const PAYLOAD_TOO_LARGE: StatusCode = StatusCode::CONTENT_TOO_LARGE;

    /// Validates and wraps a numeric status code.
    pub fn from_u16(code: u16) -> Result<StatusCode, Error> {
        if (100..=999).contains(&code) {
            Ok(StatusCode(code))
        } else {
            Err(Error::InvalidStatus(code))
        }
    }

    /// The numeric code.
    pub const fn as_u16(self) -> u16 {
        self.0
    }

    /// The reason phrase registered for this code, if it is a known one.
    pub fn canonical_reason(self) -> Option<&'static str> {
        canonical_reason(self.0)
    }

    /// `1xx`: an interim response.
    pub const fn is_informational(self) -> bool {
        self.0 >= 100 && self.0 < 200
    }

    /// `2xx`.
    pub const fn is_success(self) -> bool {
        self.0 >= 200 && self.0 < 300
    }

    /// `3xx`.
    pub const fn is_redirection(self) -> bool {
        self.0 >= 300 && self.0 < 400
    }

    /// `4xx`.
    pub const fn is_client_error(self) -> bool {
        self.0 >= 400 && self.0 < 500
    }

    /// `5xx`.
    pub const fn is_server_error(self) -> bool {
        self.0 >= 500 && self.0 < 600
    }

    /// Whether a response with this status never carries content
    /// (RFC 9110 §6.4.1: 1xx, 204 and 304).
    pub const fn forbids_body(self) -> bool {
        self.is_informational() || self.0 == 204 || self.0 == 304
    }
}

impl fmt::Display for StatusCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.canonical_reason() {
            Some(reason) => write!(f, "{} {}", self.0, reason),
            None => write!(f, "{}", self.0),
        }
    }
}

impl From<StatusCode> for u16 {
    fn from(status: StatusCode) -> u16 {
        status.0
    }
}
