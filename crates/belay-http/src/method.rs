//! Request methods (RFC 9110 §9) and protocol versions.

use std::fmt;
use std::str::FromStr;

use crate::error::Error;
use crate::headers::is_tchar;

/// A request method.
///
/// Method names are case-sensitive tokens: `get` is a valid method, just not
/// the same one as `GET`, so it parses as [`Method::Extension`].
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum Method {
    /// `GET`
    Get,
    /// `HEAD`
    Head,
    /// `POST`
    Post,
    /// `PUT`
    Put,
    /// `DELETE`
    Delete,
    /// `PATCH` (RFC 5789)
    Patch,
    /// `OPTIONS`
    Options,
    /// `TRACE`
    Trace,
    /// `CONNECT`
    Connect,
    /// Any other valid token.
    Extension(String),
}

impl Method {
    /// Parses a method token.
    ///
    /// ```
    /// use belay_http::Method;
    ///
    /// assert_eq!(Method::parse("DELETE")?, Method::Delete);
    /// assert_eq!(Method::parse("PURGE")?.as_str(), "PURGE");
    /// assert!(Method::parse("NOT A METHOD").is_err());
    /// # Ok::<(), belay_http::Error>(())
    /// ```
    pub fn parse(s: &str) -> Result<Method, Error> {
        Method::from_token(s.as_bytes()).ok_or_else(|| Error::InvalidMethod(s.to_owned()))
    }

    pub(crate) fn from_token(token: &[u8]) -> Option<Method> {
        Some(match token {
            b"GET" => Method::Get,
            b"HEAD" => Method::Head,
            b"POST" => Method::Post,
            b"PUT" => Method::Put,
            b"DELETE" => Method::Delete,
            b"PATCH" => Method::Patch,
            b"OPTIONS" => Method::Options,
            b"TRACE" => Method::Trace,
            b"CONNECT" => Method::Connect,
            _ => {
                if token.is_empty() || !token.iter().all(|&b| is_tchar(b)) {
                    return None;
                }
                // All tchars are ASCII, so this cannot fail.
                Method::Extension(String::from_utf8_lossy(token).into_owned())
            }
        })
    }

    /// The method name as sent on the wire.
    pub fn as_str(&self) -> &str {
        match self {
            Method::Get => "GET",
            Method::Head => "HEAD",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
            Method::Patch => "PATCH",
            Method::Options => "OPTIONS",
            Method::Trace => "TRACE",
            Method::Connect => "CONNECT",
            Method::Extension(name) => name,
        }
    }

    /// Safe methods are read-only by definition (RFC 9110 §9.2.1).
    pub fn is_safe(&self) -> bool {
        matches!(
            self,
            Method::Get | Method::Head | Method::Options | Method::Trace
        )
    }

    /// Idempotent methods may be retried automatically after a connection
    /// failure (RFC 9110 §9.2.2).
    pub fn is_idempotent(&self) -> bool {
        self.is_safe() || matches!(self, Method::Put | Method::Delete)
    }
}

impl FromStr for Method {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        Method::parse(s)
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The protocol version of a message. Only HTTP/1.x is spoken.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Version {
    /// HTTP/1.0: connections close after each exchange unless the peer asks
    /// for keep-alive, and chunked encoding is unavailable.
    Http10,
    /// HTTP/1.1.
    Http11,
}
