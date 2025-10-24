//! Serialization of request and response heads.
//!
//! Framing fields (`Content-Length`, `Transfer-Encoding`, `Connection`,
//! `Keep-Alive`) are owned by the writer: whatever the caller put in its
//! header map for them is dropped and replaced by values derived from the
//! actual body, so a handler cannot accidentally desynchronise the stream.

use crate::error::Error;
use crate::headers::names::{
    CONNECTION, CONTENT_LENGTH, DATE, HOST, KEEP_ALIVE, SERVER, TRANSFER_ENCODING, USER_AGENT,
};
use crate::headers::{is_valid_name, Headers};
use crate::method::Method;
use crate::status::StatusCode;

/// How a response body will be delimited on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutFraming {
    /// No framing field at all (204, 1xx, or 304 without a known length).
    None,
    /// `Content-Length: n`.
    Length(u64),
    /// `Transfer-Encoding: chunked`.
    Chunked,
    /// Delimited by closing the connection (streams to HTTP/1.0 clients).
    Close,
}

/// Everything about a response head that is not in the handler's
/// [`Headers`].
#[derive(Debug)]
pub(crate) struct ResponseMeta<'a> {
    pub framing: OutFraming,
    /// Value for the `Connection` field, if one should be sent.
    pub connection: Option<&'static str>,
    pub date: Option<&'a str>,
    pub server: Option<&'a str>,
}
