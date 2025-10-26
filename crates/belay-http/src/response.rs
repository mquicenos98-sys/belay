//! Responses and response bodies, including streamed ones.
//!
//! A [`Body::Stream`] is written with chunked encoding (or, for HTTP/1.0
//! clients, raw until the connection closes). The stream runs on the
//! connection's thread after the head has been sent, so a long-lived stream
//! occupies one connection slot for its lifetime; see
//! [`ServerConfig::max_connections`](crate::ServerConfig::max_connections).

use std::fmt;
use std::io::{self, Write};
use std::net::TcpStream;

use crate::headers::names::{CONTENT_TYPE, LOCATION};
use crate::headers::Headers;
use crate::proto::chunked::{encode_chunk, LAST_CHUNK};
use crate::status::StatusCode;
use crate::sync::ShutdownSignal;

/// Bytes buffered by [`BodyWriter`]'s `io::Write` impl before a chunk is
/// emitted without an explicit flush.
const FLUSH_THRESHOLD: usize = 16 * 1024;

/// A response body produced incrementally.
///
/// Implemented for closures, which is usually all that is needed:
///
/// ```
/// use std::io::Write;
/// use belay_http::{BodyWriter, Response};
///
/// let resp = Response::stream("text/plain", |out: &mut BodyWriter<'_>| {
///     for i in 0..3 {
///         writeln!(out, "line {i}")?;
///         out.flush()?; // one chunk per line
///     }
///     Ok(())
/// });
/// assert_eq!(resp.body.into_bytes()?, b"line 0\nline 1\nline 2\n");
/// # Ok::<(), std::io::Error>(())
/// ```
pub trait StreamBody: Send {
    /// Writes the body. Returning an error aborts the response; the
    /// connection is then closed because the message cannot be terminated
    /// cleanly.
    fn write_body(&mut self, out: &mut BodyWriter<'_>) -> io::Result<()>;
}

impl<F> StreamBody for F
where
    F: FnMut(&mut BodyWriter<'_>) -> io::Result<()> + Send,
{
    fn write_body(&mut self, out: &mut BodyWriter<'_>) -> io::Result<()> {
        self(out)
    }
}

/// A response body.
#[derive(Default)]
pub enum Body {
    /// No content.
    #[default]
    Empty,
    /// A complete body in memory, sent with `Content-Length`.
    Bytes(Vec<u8>),
    /// A body written incrementally, sent chunked.
    Stream(Box<dyn StreamBody>),
}

impl Body {
    /// The length when it is known up front.
    pub fn known_len(&self) -> Option<u64> {
        match self {
            Body::Empty => Some(0),
            Body::Bytes(bytes) => Some(bytes.len() as u64),
            Body::Stream(_) => None,
        }
    }

    /// The bytes of a non-streaming body.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Body::Empty => Some(&[]),
            Body::Bytes(bytes) => Some(bytes),
            Body::Stream(_) => None,
        }
    }

    /// Whether this is a streaming body.
    pub fn is_stream(&self) -> bool {
        matches!(self, Body::Stream(_))
    }

    /// Collects the body into memory, running a stream to completion. Meant
    /// for tests and for bodies known to be finite.
    pub fn into_bytes(self) -> io::Result<Vec<u8>> {
        match self {
            Body::Empty => Ok(Vec::new()),
            Body::Bytes(bytes) => Ok(bytes),
            Body::Stream(mut stream) => {
                let mut out = Vec::new();
                let mut writer = BodyWriter::new(&mut out);
                stream.write_body(&mut writer)?;
                writer.flush()?;
                drop(writer);
                Ok(out)
            }
        }
    }
}

impl fmt::Debug for Body {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Body::Empty => f.write_str("Empty"),
            Body::Bytes(bytes) => write!(f, "Bytes({} bytes)", bytes.len()),
            Body::Stream(_) => f.write_str("Stream"),
        }
    }
}

impl From<Vec<u8>> for Body {
    fn from(bytes: Vec<u8>) -> Self {
        Body::Bytes(bytes)
    }
}

impl From<String> for Body {
    fn from(text: String) -> Self {
        Body::Bytes(text.into_bytes())
    }
}

impl From<&str> for Body {
    fn from(text: &str) -> Self {
        Body::Bytes(text.as_bytes().to_vec())
    }
}

impl From<&[u8]> for Body {
    fn from(bytes: &[u8]) -> Self {
        Body::Bytes(bytes.to_vec())
    }
}

/// The sink a [`StreamBody`] writes into.
///
/// `io::Write::write` buffers; [`flush`](Write::flush) (or [`send`](Self::send))
/// emits everything buffered as one chunk and pushes it to the socket, so
/// the client sees data at flush boundaries. Writes are bounded by the
/// server's write timeout: a client that stops reading makes them fail
/// instead of blocking forever.
pub struct BodyWriter<'a> {
    out: &'a mut dyn Write,
    peer: Option<&'a TcpStream>,
    shutdown: Option<ShutdownSignal>,
    chunked: bool,
    buf: Vec<u8>,
    frame: Vec<u8>,
    written: u64,
}
